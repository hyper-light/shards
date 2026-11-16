//! A network's addresses and names held across processes (D285, slice 1): every process of
//! a home that puts runs on a network (its daemon, each orchestrator's run hosts) takes them
//! from one record in the home, under one lock, each held by the process that took it for as
//! long as that process lives.
//!
//! A lease is a file made only where none is (`create_new`), its record written, then held
//! shared (flock(2), LockFileEx: [`Locked::try_share`]) by its holder until it drops or its
//! holder ends, however it ends: the kernel lets a lock go as the last descriptor of its open
//! file closes. A lease no process holds is stale; the next look under the home's networks
//! lock ([`hold`]) finds it so at once, its exclusive try taking the lock ([`Locked::try_hold`]),
//! and removes it. No clock and no process ID decides it. Holders hold shared and lookers try
//! exclusive because Windows keeps every other handle from reading a range held exclusively
//! (LockFileEx), and a looker reads what a live lease says. The home's lock is the only lock
//! any of this waits for, and no caller holds it across another process's answer.
//!
//! ```text
//! networks/lock                          the home's networks held still (hold)
//! networks/ID.leases/addr/A.B.C.D        an address, held by the run on it: its holder's ID
//! networks/ID.leases/addr6/HEX           an IPv6 address, its 128 bits in hex: its holder's ID
//! networks/ID.leases/name/@NAME          a member's name, one member's: the address it names
//! networks/ID.leases/alias/@NAME/A.B.C.D an alias, shared: an address that answers to it
//! networks/ID.leases/pairer              the daemon linking the network's members (Pairing)
//! networks/ID.leases/pairers/HOLDER      a run host of the orchestrator linking them: its token
//! ```
//!
//! Names follow Docker's rules (ROOT, 2026-11-16): a member's own name, a container's, is one
//! member's; an alias is shared, Compose's scaled replicas answering to one each at its own
//! address (moby f78c987a, daemon/names/names.go's grammar; libnetwork's service records, a
//! name to every address that has it).

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use shards_image::store::Locked;

use super::{Pool, Pool6};

/// The home's networks lock's file, in its `networks/`.
const LOCK: &str = "lock";
/// The file a network's pairing holds, in its leases.
const PAIRER: &str = "pairer";
/// A network's address leases' directory.
const ADDRESSES: &str = "addr";
/// A network's IPv6 address leases' directory (D99's members' second addresses).
const ADDRESSES6: &str = "addr6";
/// An IPv6 address lease's file name: the address's 128 bits as lowercase hex digits, four
/// bits each, as Windows takes no `:` in a file's name (Microsoft, "Naming Files, Paths, and
/// Namespaces").
const HEX6: usize = 2 * std::mem::size_of::<u128>();
/// A network's name leases' directory.
const NAMES: &str = "name";
/// A network's aliases' directory: a directory for each alias, a lease in it for each address
/// that answers to it.
const ALIASES: &str = "alias";
/// The leases of an orchestrator's run hosts that link a network's members, each by its
/// holder's ID, saying its orchestrator's token.
const PAIRERS: &str = "pairers";
/// The longest name a lease holds: a whole name in the DNS (RFC 1035 §2.3.4, 255 octets on
/// the wire, 253 in text), so that every name a network's resolver could be asked fits, and
/// with `@` its file's name fits each supported filesystem's 255 bytes.
const NAME_LEN: usize = 253;
/// What begins a name lease's file: Windows takes a base name of `CON`, `PRN`, `AUX`, `NUL`,
/// `COM1`-`COM9` or `LPT1`-`LPT9`, with any extension, for a device (Microsoft, "Naming Files,
/// Paths, and Namespaces"), each of them a host's name too; `@` begins none, nor any host's
/// name.
const NAME_MARK: char = '@';
/// A holder's ID: a microVM ID's form ([`crate::containers::new_id`]), its random bytes as
/// lowercase hex digits; a network's ID, which names its leases' directory, is one too.
const ID_LEN: usize = 2 * crate::containers::ID_BYTES;
/// The longest IPv4 address in dotted decimal: what a name lease says.
const ADDRESS_LEN: usize = "255.255.255.255".len();

/// Why a lease was not given, or a network's leases not cleared (R04): callers match the
/// cause, its `Display` words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The address asked is in use: a live lease's, or one its pool keeps (its network's and
    /// broadcast addresses, its gateway, an auxiliary address), as dockerd says it.
    AddressTaken(IpAddr),
    /// The address asked lies outside the pool's subnet: its range bounds only the addresses
    /// given unasked, as libnetwork's (moby f78c987a, ipams/defaultipam/address_space.go:350-353,
    /// allocator.go:315-319).
    OutOfRange(IpAddr),
    /// Every address of the pool's range is in use.
    Exhausted,
    /// The name is a live lease's on the network, whatever its case: a member's, or an alias
    /// its address answers to already.
    NameTaken(String),
    /// Neither a host's name (RFC 1035 §2.3.1, RFC 1123 §2.1) nor a container's (Docker's
    /// `[a-zA-Z0-9][a-zA-Z0-9_.-]+`), or longer than [`NAME_LEN`], or ending in `.`, which
    /// Windows drops from a file's name (Microsoft, "Naming Files, Paths, and Namespaces").
    NotAName(String),
    /// An ID (a holder's, a network's) that is not a microVM ID's form.
    NotAnId,
    /// The network is in use: a lease of it lives, or a process links its members.
    InUse,
    /// Another process links the network's members, or another open of this one's does.
    PairedElsewhere,
    /// The home's files refused: which, and the error's kind.
    Io { what: &'static str, kind: io::ErrorKind },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::AddressTaken(ip) => write!(f, "{ip}: Address already in use"),
            Refused::OutOfRange(ip) => write!(f, "{ip}: not in the network's subnet"),
            Refused::Exhausted => f.write_str("no address left in the network's range"),
            Refused::NameTaken(name) => write!(f, "{name}: another member's name on the network"),
            Refused::NotAName(name) => write!(
                f,
                "{name:?}: not a host's name nor a container's ([a-zA-Z0-9][a-zA-Z0-9_.-]+, 253 bytes at most, no last dot)"
            ),
            Refused::NotAnId => write!(f, "not an ID: {ID_LEN} lowercase hex digits"),
            Refused::InUse => f.write_str("in use: a lease of it lives, or a process links its members"),
            Refused::PairedElsewhere => f.write_str("another process links its members"),
            Refused::Io { what, kind } => write!(f, "{what}: {kind}"),
        }
    }
}

impl std::error::Error for Refused {}

fn io_err(what: &'static str) -> impl Fn(io::Error) -> Refused {
    move |e| Refused::Io { what, kind: e.kind() }
}

/// The home's networks held still: every change of a network or of its leases, in any process
/// of the home, waits for it to drop.
#[derive(Debug)]
pub struct Held {
    _lock: Locked,
}

/// The home's networks lock, waited for: let go as it drops, or as its holder ends.
pub fn hold(home: &Path) -> Result<Held, Refused> {
    let dir = home.join("networks");
    super::make_dir(&dir).map_err(io_err("the home's networks"))?;
    Locked::exclusive(&dir.join(LOCK))
        .map(|lock| Held { _lock: lock })
        .map_err(io_err("the home's networks lock"))
}

/// A network's leases ([`Leases::of`]).
#[derive(Debug)]
pub struct Leases {
    dir: PathBuf,
}

/// A run's address on a network, IPv4 or IPv6 (`A`), held by this process for as long as it
/// lives: let go as it drops (its file removed, then its lock), or as the process ends (its
/// lock let go by the kernel, its file then stale).
#[derive(Debug)]
pub struct Lease<A = Ipv4Addr> {
    ip: A,
    path: PathBuf,
    _held: Locked,
}

impl<A: Copy> Lease<A> {
    pub fn ip(&self) -> A {
        self.ip
    }
}

/// A member's name on a network, naming its run's address, held as a [`Lease`] is.
#[derive(Debug)]
pub struct Name {
    name: String,
    path: PathBuf,
    _held: Locked,
}

impl Name {
    pub fn name(&self) -> &str {
        &self.name
    }
}

// Removed while still held, so that no look finds it stale while its holder lives; its lock
// goes after, as its fields drop.
impl<A> Drop for Lease<A> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Name {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The process that links a network's members (D285: one at a time, so that its members are
/// never split between two that cannot link them), until it drops or the process ends: the
/// daemon's, or one run host's share of its orchestrator's ([`Leases::pair_as`]), its lease
/// removed as it drops.
#[derive(Debug)]
pub struct Pairing {
    path: Option<PathBuf>,
    _held: Locked,
}

impl Drop for Pairing {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Leases {
    /// Network `id`'s leases in `home`: refused [`Refused::NotAnId`] for an ID that is not
    /// one, which could name another path.
    pub fn of(home: &Path, id: &str) -> Result<Leases, Refused> {
        if !is_id(id) {
            return Err(Refused::NotAnId);
        }
        Ok(Leases {
            dir: home.join("networks").join(format!("{id}.leases")),
        })
    }

    /// Each address held now, with its holder's ID (none where its record is not one); each
    /// lease no process holds removed.
    pub fn addresses(&self, _held: &Held) -> Result<Vec<(Ipv4Addr, Option<String>)>, Refused> {
        Ok(live(&self.dir.join(ADDRESSES), ID_LEN)?
            .into_iter()
            .filter_map(|(file, said)| Some((file.parse::<Ipv4Addr>().ok()?, holder_of(said))))
            .collect())
    }

    /// Each IPv6 address held now, as [`Leases::addresses`] says each IPv4 one.
    pub fn addresses6(&self, _held: &Held) -> Result<Vec<(Ipv6Addr, Option<String>)>, Refused> {
        Ok(live(&self.dir.join(ADDRESSES6), ID_LEN)?
            .into_iter()
            .filter_map(|(file, said)| Some((parse6(&file)?, holder_of(said))))
            .collect())
    }

    /// Each name held now, with the address it names (none where its record is not one):
    /// each member's, then each alias at each address that answers to it; each lease no
    /// process holds removed.
    pub fn names(&self, _held: &Held) -> Result<Vec<(String, Option<Ipv4Addr>)>, Refused> {
        let mut names: Vec<(String, Option<Ipv4Addr>)> = live(&self.dir.join(NAMES), ADDRESS_LEN)?
            .into_iter()
            .filter_map(|(file, said)| {
                let name = file.strip_prefix(NAME_MARK)?.to_string();
                let ip = std::str::from_utf8(&said)
                    .ok()
                    .and_then(|s| s.parse::<Ipv4Addr>().ok());
                Some((name, ip))
            })
            .collect();
        let aliases = match std::fs::read_dir(self.dir.join(ALIASES)) {
            Ok(aliases) => aliases,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(names),
            Err(e) => return Err(io_err("a network's aliases")(e)),
        };
        for alias in aliases {
            let alias = alias.map_err(io_err("a network's aliases"))?;
            let Some(name) = alias
                .file_name()
                .to_str()
                .and_then(|f| f.strip_prefix(NAME_MARK))
                .map(str::to_string)
            else {
                continue;
            };
            for (file, _) in live(&alias.path(), 0)? {
                names.push((name.clone(), file.parse::<Ipv4Addr>().ok()));
            }
        }
        Ok(names)
    }

    /// An address of `pool` held for `holder` from now on: `asked`, anywhere in the pool's
    /// subnet, or the lowest free in its range (libnetwork's local IPAM, as
    /// [`super::allocate`] gives one: an address let go is the next given, D46).
    pub fn take(
        &self,
        held: &Held,
        pool: &Pool,
        asked: Option<Ipv4Addr>,
        holder: &str,
    ) -> Result<Lease, Refused> {
        if !is_id(holder) {
            return Err(Refused::NotAnId);
        }
        let leased: Vec<Ipv4Addr> = self.addresses(held)?.into_iter().map(|(ip, _)| ip).collect();
        let ip = match asked {
            Some(ip) if !super::contains(pool.subnet, ip) => return Err(Refused::OutOfRange(ip.into())),
            Some(ip) if super::in_use(pool, &leased).contains(&ip) => {
                return Err(Refused::AddressTaken(ip.into()));
            }
            Some(ip) => ip,
            None => super::allocate(pool, &leased).ok_or(Refused::Exhausted)?,
        };
        self.lease(ADDRESSES, &ip.to_string(), ip, holder)
    }

    /// An IPv6 address of `pool` held for `holder` from now on, as [`Leases::take`] gives an
    /// IPv4 one ([`super::allocate6`]'s, D99).
    pub fn take6(
        &self,
        held: &Held,
        pool: &Pool6,
        asked: Option<Ipv6Addr>,
        holder: &str,
    ) -> Result<Lease<Ipv6Addr>, Refused> {
        if !is_id(holder) {
            return Err(Refused::NotAnId);
        }
        let leased: Vec<Ipv6Addr> = self.addresses6(held)?.into_iter().map(|(ip, _)| ip).collect();
        let ip = match asked {
            Some(ip) if !super::prefix6(&pool.subnet).is_some_and(|subnet| super::contains6(subnet, ip)) => {
                return Err(Refused::OutOfRange(ip.into()));
            }
            Some(ip) if super::in_use6(pool, &leased).contains(&ip) => {
                return Err(Refused::AddressTaken(ip.into()));
            }
            Some(ip) => ip,
            None => super::allocate6(pool, &leased).ok_or(Refused::Exhausted)?,
        };
        self.lease(ADDRESSES6, &file6(ip), ip, holder)
    }

    /// `ip`'s lease, file `file` of directory `kind`, made for `holder`: refused
    /// [`Refused::AddressTaken`] where a live lease has it.
    fn lease<A: Copy + Into<IpAddr>>(
        &self,
        kind: &str,
        file: &str,
        ip: A,
        holder: &str,
    ) -> Result<Lease<A>, Refused> {
        let dir = self.dir.join(kind);
        make_dirs(&dir)?;
        let (path, lock) = made(&dir, file, holder.as_bytes())?.ok_or(Refused::AddressTaken(ip.into()))?;
        Ok(Lease {
            ip,
            path,
            _held: lock,
        })
    }

    /// `name` on the network, a member's own and one member's (a container's name, Docker's
    /// rule), lowercased (RFC 4343), naming `lease`'s address, held from now on: refused where
    /// a live lease holds it.
    pub fn name(&self, _held: &Held, lease: &Lease, name: &str) -> Result<Name, Refused> {
        let name = named(name)?;
        let dir = self.dir.join(NAMES);
        make_dirs(&dir)?;
        let file = format!("{NAME_MARK}{name}");
        match made(&dir, &file, lease.ip.to_string().as_bytes())? {
            Some((path, lock)) => Ok(Name {
                name,
                path,
                _held: lock,
            }),
            None => Err(Refused::NameTaken(name)),
        }
    }

    /// `name` as an alias `lease`'s address answers to on the network, lowercased, held from
    /// now on: shared, every address that holds it answered (Docker's rule for an alias,
    /// Compose's scaled replicas each at its own); refused [`Refused::NameTaken`] where
    /// `lease`'s address holds it already.
    pub fn alias(&self, _held: &Held, lease: &Lease, name: &str) -> Result<Name, Refused> {
        let name = named(name)?;
        let dir = self.dir.join(ALIASES).join(format!("{NAME_MARK}{name}"));
        make_dirs(&self.dir.join(ALIASES))?;
        super::make_dir(&dir).map_err(io_err("a network's aliases"))?;
        match made(&dir, &lease.ip.to_string(), &[])? {
            Some((path, lock)) => Ok(Name {
                name,
                path,
                _held: lock,
            }),
            None => Err(Refused::NameTaken(name)),
        }
    }

    /// The network's pairing, shared by the run hosts of the orchestrator whose token is
    /// `pairer` (D285 slice 4: its process makes their links), this run host's, `holder`, from
    /// now on: refused [`Refused::PairedElsewhere`] where the daemon pairs it, where another
    /// orchestrator's run host holds it, or where `holder` holds it already. Its lease says
    /// its orchestrator's token; a run host's end lets it go with its other leases.
    pub fn pair_as(&self, held: &Held, pairer: &str, holder: &str) -> Result<Pairing, Refused> {
        if !is_id(pairer) || !is_id(holder) {
            return Err(Refused::NotAnId);
        }
        if self.paired(held)? {
            return Err(Refused::PairedElsewhere);
        }
        let others = live(&self.dir.join(PAIRERS), ID_LEN)?;
        if others
            .iter()
            .any(|(_, said)| said.as_slice() != pairer.as_bytes())
        {
            return Err(Refused::PairedElsewhere);
        }
        let dir = self.dir.join(PAIRERS);
        make_dirs(&dir)?;
        let (path, lock) = made(&dir, holder, pairer.as_bytes())?.ok_or(Refused::PairedElsewhere)?;
        Ok(Pairing {
            path: Some(path),
            _held: lock,
        })
    }

    /// Whether the daemon pairs the network now: its pairer's file held.
    fn paired(&self, _held: &Held) -> Result<bool, Refused> {
        match File::open(self.dir.join(PAIRER)) {
            Ok(file) => Ok(Locked::try_hold(file)
                .map_err(io_err("a network's pairer"))?
                .is_none()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err("a network's pairer")(e)),
        }
    }

    /// The network's pairing, this process's from now on: refused
    /// [`Refused::PairedElsewhere`] while another open of it holds it, this process's own
    /// among them, so that a process holds one pairing of a network, which its runs share;
    /// and while an orchestrator's run hosts pair it ([`Leases::pair_as`]).
    pub fn pair(&self, _held: &Held) -> Result<Pairing, Refused> {
        if !live(&self.dir.join(PAIRERS), ID_LEN)?.is_empty() {
            return Err(Refused::PairedElsewhere);
        }
        super::make_dir(&self.dir).map_err(io_err("a network's leases"))?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(PAIRER))
            .map_err(io_err("a network's pairer"))?;
        Locked::try_hold(file)
            .map_err(io_err("a network's pairer"))?
            .map(|held| Pairing {
                path: None,
                _held: held,
            })
            .ok_or(Refused::PairedElsewhere)
    }

    /// The network's leases removed, as the network is: refused [`Refused::InUse`] where a
    /// lease of it lives or a process links its members.
    pub fn clear(&self, held: &Held) -> Result<(), Refused> {
        if !self.addresses(held)?.is_empty()
            || !self.addresses6(held)?.is_empty()
            || !self.names(held)?.is_empty()
            || !live(&self.dir.join(PAIRERS), ID_LEN)?.is_empty()
        {
            return Err(Refused::InUse);
        }
        match File::open(self.dir.join(PAIRER)) {
            Ok(file) => {
                if Locked::try_hold(file)
                    .map_err(io_err("a network's pairer"))?
                    .is_none()
                {
                    return Err(Refused::InUse);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("a network's pairer")(e)),
        }
        match std::fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err("a network's leases")(e)),
            _ => Ok(()),
        }
    }
}

/// Whether `id` is a microVM ID's form: [`ID_LEN`] lowercase hex digits.
fn is_id(id: &str) -> bool {
    lower_hex(id, ID_LEN)
}

/// `name` as a lease holds it, lowercased (RFC 4343): a host's name or a container's, at most
/// [`NAME_LEN`] bytes, its last not `.`; refused [`Refused::NotAName`] otherwise.
fn named(name: &str) -> Result<String, Refused> {
    let name = name.to_ascii_lowercase();
    let container = !name.starts_with('/') && crate::containers::valid_name(&name);
    if (shards_net::dns::host_name(&name) || container) && name.len() <= NAME_LEN && !name.ends_with('.') {
        Ok(name)
    } else {
        Err(Refused::NotAName(name))
    }
}

/// Whether `s` is `len` lowercase hex digits.
fn lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What an address lease says of its holder: its ID, none where it says no ID.
fn holder_of(said: Vec<u8>) -> Option<String> {
    String::from_utf8(said).ok().filter(|h| is_id(h))
}

/// An IPv6 address lease's file name ([`HEX6`]).
fn file6(ip: Ipv6Addr) -> String {
    format!("{:0width$x}", u128::from(ip), width = HEX6)
}

/// The IPv6 address an address lease's file names: none where it is no [`file6`] name.
fn parse6(file: &str) -> Option<Ipv6Addr> {
    if !lower_hex(file, HEX6) {
        return None;
    }
    u128::from_str_radix(file, 16).ok().map(Ipv6Addr::from)
}

/// `dir`, a kind of lease's directory, made where it is not, with its network's leases'.
fn make_dirs(dir: &Path) -> Result<(), Refused> {
    if let Some(leases) = dir.parent() {
        super::make_dir(leases).map_err(io_err("a network's leases"))?;
    }
    super::make_dir(dir).map_err(io_err("a network's leases"))
}

/// Each lease of `dir` a process holds, by its file's name, with what it says (at most `cap`
/// bytes of it, its kind's longest record: past that it says nothing); each no process holds
/// removed.
fn live(dir: &Path, cap: usize) -> Result<Vec<(String, Vec<u8>)>, Refused> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("a network's leases")(e)),
    };
    let mut held = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err("a network's leases"))?;
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = entry.path();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("a lease")(e)),
        };
        // Read first: a live lease's record is read past its holder's shared lock, as a
        // stale one's is not needed.
        let mut said = Vec::new();
        let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
        (&file)
            .take(limit)
            .read_to_end(&mut said)
            .map_err(io_err("a lease's record"))?;
        if said.len() > cap {
            said.clear();
        }
        match Locked::try_hold(file).map_err(io_err("a lease's lock"))? {
            // No process holds it: its holder has ended, and with it the lease.
            Some(stale) => {
                remove(&path)?;
                drop(stale);
            }
            None => held.push((name, said)),
        }
    }
    Ok(held)
}

/// The lease `file` of `dir`, made now saying `says` and held shared from now on: none where
/// a live lease has it; one no process holds is its ended holder's, removed and made anew.
fn made(dir: &Path, file: &str, says: &[u8]) -> Result<Option<(PathBuf, Locked)>, Refused> {
    let path = dir.join(file);
    let created = match create(&path, says)? {
        Some(created) => Some(created),
        None if stale_removed(&path)? => create(&path, says)?,
        None => None,
    };
    let Some(created) = created else {
        return Ok(None);
    };
    match Locked::try_share(created) {
        Ok(Some(held)) => Ok(Some((path, held))),
        // Made under the home's lock, no other open of it can hold it: a lock refused is the
        // filesystem's, and the file goes with it.
        Ok(None) => {
            let _ = std::fs::remove_file(&path);
            Err(Refused::Io {
                what: "a lease's lock",
                kind: io::ErrorKind::WouldBlock,
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(io_err("a lease's lock")(e))
        }
    }
}

/// `path` made now, none there, saying `says`, written before its holder's shared lock,
/// which keeps every process from writing it (LockFileEx): none where a file is there.
fn create(path: &Path, says: &[u8]) -> Result<Option<File>, Refused> {
    match File::options().write(true).create_new(true).open(path) {
        Ok(mut file) => match file.write_all(says) {
            Ok(()) => Ok(Some(file)),
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(path);
                Err(io_err("a lease's record")(e))
            }
        },
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(io_err("a lease")(e)),
    }
}

/// Whether the lease at `path` is free now: none there, or none holding it, its file then
/// removed; false where a process holds it.
fn stale_removed(path: &Path) -> Result<bool, Refused> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(io_err("a lease")(e)),
    };
    match Locked::try_hold(file).map_err(io_err("a lease's lock"))? {
        None => Ok(false),
        Some(stale) => {
            remove(path)?;
            drop(stale);
            Ok(true)
        }
    }
}

/// `path` removed; gone already is the same.
fn remove(path: &Path) -> Result<(), Refused> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err("a stale lease")(e)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn id(c: char) -> String {
        c.to_string().repeat(ID_LEN)
    }

    /// The pool a network made on `subnet` is given (D46): its gateway the subnet's first free
    /// address.
    fn pool(subnet: &str) -> Pool {
        super::super::new_pool(
            super::super::parse_prefix(subnet),
            None,
            None,
            &BTreeMap::new(),
            &[],
        )
        .unwrap()
    }

    /// A home, held, and network `e…e`'s leases in it.
    fn network(name: &str) -> (shards_testdir::TempDir, Held, Leases) {
        let home = shards_testdir::TempDir::new(name).unwrap();
        let held = hold(&home).unwrap();
        let leases = Leases::of(&home, &id('e')).unwrap();
        (home, held, leases)
    }

    fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
        v.sort();
        v
    }

    /// Addresses are the lowest free of the range, past the gateway, as D46's are: one let go
    /// is the next given, and each live one is read back with its holder.
    #[test]
    fn addresses_are_the_lowest_free_and_one_let_go_is_the_next_given() {
        let (_home, held, leases) = network("leases-lowest");
        let range = pool("10.89.0.0/24");
        let a = leases.take(&held, &range, None, &id('a')).unwrap();
        let b = leases.take(&held, &range, None, &id('b')).unwrap();
        assert_eq!(
            (a.ip(), b.ip()),
            (Ipv4Addr::new(10, 89, 0, 2), Ipv4Addr::new(10, 89, 0, 3))
        );
        assert_eq!(
            sorted(leases.addresses(&held).unwrap()),
            vec![(a.ip(), Some(id('a'))), (b.ip(), Some(id('b')))]
        );
        drop(a);
        let c = leases.take(&held, &range, None, &id('c')).unwrap();
        assert_eq!(c.ip(), Ipv4Addr::new(10, 89, 0, 2));
    }

    /// A lease whose holder ended: its file left, no lock on it (the kernel let its holder's go
    /// with its last descriptor). The next look removes it, and its address is given again.
    #[test]
    fn a_lease_no_process_holds_is_removed_and_its_address_given_again() {
        let (home, held, leases) = network("leases-stale");
        let range = pool("10.89.0.0/24");
        let dir = home
            .join("networks")
            .join(format!("{}.leases", id('e')))
            .join(ADDRESSES);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("10.89.0.2"), id('d')).unwrap();
        assert!(leases.addresses(&held).unwrap().is_empty());
        assert!(!dir.join("10.89.0.2").exists(), "the stale lease was kept");
        std::fs::write(dir.join("10.89.0.2"), id('d')).unwrap();
        let a = leases
            .take(&held, &range, Some(Ipv4Addr::new(10, 89, 0, 2)), &id('a'))
            .unwrap();
        assert_eq!(a.ip(), Ipv4Addr::new(10, 89, 0, 2));
        assert_eq!(leases.addresses(&held).unwrap(), vec![(a.ip(), Some(id('a')))]);
    }

    /// An address a live lease holds, the gateway, and one outside the subnet are refused,
    /// each by its cause; the full range is refused as such.
    #[test]
    fn an_address_in_use_or_out_of_range_is_refused_by_its_cause() {
        let (_home, held, leases) = network("leases-refused");
        let range = pool("10.89.0.0/24");
        let five = Ipv4Addr::new(10, 89, 0, 5);
        let a = leases.take(&held, &range, Some(five), &id('a')).unwrap();
        assert_eq!(a.ip(), five);
        assert_eq!(
            leases.take(&held, &range, Some(five), &id('b')).unwrap_err(),
            Refused::AddressTaken(five.into())
        );
        let gateway = Ipv4Addr::new(10, 89, 0, 1);
        assert_eq!(
            leases.take(&held, &range, Some(gateway), &id('b')).unwrap_err(),
            Refused::AddressTaken(gateway.into())
        );
        let outside = Ipv4Addr::new(10, 90, 0, 5);
        assert_eq!(
            leases.take(&held, &range, Some(outside), &id('b')).unwrap_err(),
            Refused::OutOfRange(outside.into())
        );
        assert_eq!(
            leases.take(&held, &range, None, "not-an-id").unwrap_err(),
            Refused::NotAnId
        );
        // A /30: its network's and broadcast addresses and its gateway leave one.
        let small = pool("10.2.0.0/30");
        let (_home, held, leases) = network("leases-full");
        let only = leases.take(&held, &small, None, &id('a')).unwrap();
        assert_eq!(only.ip(), Ipv4Addr::new(10, 2, 0, 2));
        assert_eq!(
            leases.take(&held, &small, None, &id('b')).unwrap_err(),
            Refused::Exhausted
        );
    }

    /// An address asked for is given anywhere in its subnet, the range bounding only those
    /// given unasked, as libnetwork gives them (moby f78c987a, ipams/defaultipam
    /// address_space.go:350-353, allocator.go:315-319): `--ip` beside `--ip-range`.
    #[test]
    fn an_asked_address_is_given_anywhere_in_its_subnet() {
        let (_home, held, leases) = network("leases-subnet");
        let ranged = super::super::new_pool(
            super::super::parse_prefix("10.89.0.0/24"),
            super::super::parse_prefix("10.89.0.128/25"),
            None,
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        let low = Ipv4Addr::new(10, 89, 0, 9);
        let a = leases.take(&held, &ranged, Some(low), &id('a')).unwrap();
        assert_eq!(a.ip(), low);
        // The range's first address is its gateway: the next is given.
        let b = leases.take(&held, &ranged, None, &id('b')).unwrap();
        assert_eq!(b.ip(), Ipv4Addr::new(10, 89, 0, 129));
    }

    /// IPv6 addresses are leased as IPv4 ones are, each file its address's 128 bits in hex:
    /// the lowest free past the subnet's first and the gateway, one in use or outside the
    /// subnet refused by its cause, one let go the next given, and a lease no process holds
    /// removed.
    #[test]
    fn ipv6_addresses_are_leased_as_ipv4_ones_are() {
        let (home, held, leases) = network("leases-six");
        let pool6 = Pool6 {
            subnet: "fd00:1::/64".into(),
            ip_range: String::new(),
            gateway: "fd00:1::1".into(),
            aux: BTreeMap::new(),
        };
        let two: Ipv6Addr = "fd00:1::2".parse().unwrap();
        let a = leases.take6(&held, &pool6, None, &id('a')).unwrap();
        assert_eq!(a.ip(), two);
        assert_eq!(leases.addresses6(&held).unwrap(), vec![(two, Some(id('a')))]);
        assert_eq!(
            leases.take6(&held, &pool6, Some(two), &id('b')).unwrap_err(),
            Refused::AddressTaken(two.into())
        );
        let outside: Ipv6Addr = "fd00:2::5".parse().unwrap();
        assert_eq!(
            leases.take6(&held, &pool6, Some(outside), &id('b')).unwrap_err(),
            Refused::OutOfRange(outside.into())
        );
        assert_eq!(leases.clear(&held).unwrap_err(), Refused::InUse);
        drop(a);
        let c = leases.take6(&held, &pool6, None, &id('c')).unwrap();
        assert_eq!(c.ip(), two);
        drop(c);
        let file = home
            .join("networks")
            .join(format!("{}.leases", id('e')))
            .join(ADDRESSES6)
            .join(file6(two));
        assert_eq!(
            file.file_name().and_then(|n| n.to_str()),
            Some("fd000001000000000000000000000002")
        );
        std::fs::write(&file, id('d')).unwrap();
        assert!(leases.addresses6(&held).unwrap().is_empty());
        assert!(!file.exists(), "the stale lease was kept");
    }

    /// A name is held once on a network, whatever its case, until it is let go or its holder
    /// ends; a container's name (Docker's grammar, `_` and `.` among it) is held as a host's
    /// is; one that is neither, too long, or ending in `.` is refused.
    #[test]
    fn a_name_is_held_once_on_a_network_whatever_its_case() {
        let (home, held, leases) = network("leases-names");
        let range = pool("10.89.0.0/24");
        let a = leases.take(&held, &range, None, &id('a')).unwrap();
        let b = leases.take(&held, &range, None, &id('b')).unwrap();
        let db = leases.name(&held, &a, "DB").unwrap();
        assert_eq!(db.name(), "db");
        assert_eq!(
            leases.name(&held, &b, "db").unwrap_err(),
            Refused::NameTaken("db".into())
        );
        assert_eq!(
            leases.names(&held).unwrap(),
            vec![("db".to_string(), Some(a.ip()))]
        );
        let long = "a".repeat(NAME_LEN + 1);
        for bad in ["", "-db", "_db", "db.", "/db", "a/b", "a\\b", long.as_str()] {
            assert_eq!(
                leases.name(&held, &b, bad).unwrap_err(),
                Refused::NotAName(bad.to_ascii_lowercase())
            );
        }
        for docker in ["Web_1", "proj.web.1", "1234", "db-"] {
            assert_eq!(
                leases.name(&held, &b, docker).unwrap().name(),
                docker.to_ascii_lowercase()
            );
        }
        // A device's name on Windows is a host's name, held as any other.
        assert_eq!(leases.name(&held, &b, "con").unwrap().name(), "con");
        drop(db);
        let again = leases.name(&held, &b, "db").unwrap();
        assert_eq!(again.name(), "db");
        // One its holder left behind, held by no one, is given again.
        let dir = home
            .join("networks")
            .join(format!("{}.leases", id('e')))
            .join(NAMES);
        std::fs::write(dir.join("@cache"), "10.89.0.9").unwrap();
        assert_eq!(leases.name(&held, &a, "cache").unwrap().name(), "cache");
    }

    /// An alias is answered at every address that holds it, Compose's scaled replicas each at
    /// its own (Docker's rule); one address holds it once; a member's own name beside it is
    /// one member's still; and an alias let go is no longer answered at its address.
    #[test]
    fn an_alias_is_answered_at_every_address_that_holds_it() {
        let (_home, held, leases) = network("leases-alias");
        let range = pool("10.89.0.0/24");
        let a = leases.take(&held, &range, None, &id('a')).unwrap();
        let b = leases.take(&held, &range, None, &id('b')).unwrap();
        let web_a = leases.alias(&held, &a, "Web").unwrap();
        let web_b = leases.alias(&held, &b, "web").unwrap();
        assert_eq!(web_a.name(), "web");
        assert_eq!(
            leases.alias(&held, &a, "web").unwrap_err(),
            Refused::NameTaken("web".into())
        );
        let own = leases.name(&held, &a, "proj-web-1").unwrap();
        assert_eq!(
            leases.name(&held, &b, "proj-web-1").unwrap_err(),
            Refused::NameTaken("proj-web-1".into())
        );
        assert_eq!(
            sorted(leases.names(&held).unwrap()),
            vec![
                ("proj-web-1".to_string(), Some(a.ip())),
                ("web".to_string(), Some(a.ip())),
                ("web".to_string(), Some(b.ip())),
            ]
        );
        drop(web_b);
        assert_eq!(
            sorted(leases.names(&held).unwrap()),
            vec![
                ("proj-web-1".to_string(), Some(a.ip())),
                ("web".to_string(), Some(a.ip())),
            ]
        );
        assert_eq!(leases.clear(&held).unwrap_err(), Refused::InUse);
        drop((own, web_a));
    }

    /// An orchestrator's run hosts share a network's pairing, each its own lease saying their
    /// orchestrator's token (D285 slice 4): another orchestrator's run host and the daemon are
    /// refused while one holds it, and a network so paired is in use; once the last lets go,
    /// the daemon pairs it, and no run host does meanwhile.
    #[test]
    fn an_orchestrators_run_hosts_share_a_pairing_no_other_process_takes() {
        let (_home, held, leases) = network("leases-pair-as");
        let (ours, theirs) = (id('1'), id('2'));
        let a = leases.pair_as(&held, &ours, &id('a')).unwrap();
        let b = leases.pair_as(&held, &ours, &id('b')).unwrap();
        assert_eq!(
            leases.pair_as(&held, &ours, &id('a')).unwrap_err(),
            Refused::PairedElsewhere
        );
        assert_eq!(
            leases.pair_as(&held, &theirs, &id('c')).unwrap_err(),
            Refused::PairedElsewhere
        );
        assert_eq!(leases.pair(&held).unwrap_err(), Refused::PairedElsewhere);
        assert_eq!(leases.clear(&held).unwrap_err(), Refused::InUse);
        assert_eq!(
            leases.pair_as(&held, "not-a-token", &id('c')).unwrap_err(),
            Refused::NotAnId
        );
        drop((a, b));
        let daemon = leases.pair(&held).unwrap();
        assert_eq!(
            leases.pair_as(&held, &theirs, &id('c')).unwrap_err(),
            Refused::PairedElsewhere
        );
        drop(daemon);
        assert!(leases.pair_as(&held, &theirs, &id('c')).is_ok());
    }

    /// One open of a network's pairer holds it at a time, as one process's would against
    /// another's: a second is refused, and the first's drop lets the next pair.
    #[test]
    fn one_open_links_a_networks_members_at_a_time() {
        let (_home, held, leases) = network("leases-pair");
        let pairing = leases.pair(&held).unwrap();
        assert_eq!(leases.pair(&held).unwrap_err(), Refused::PairedElsewhere);
        drop(pairing);
        assert!(leases.pair(&held).is_ok());
    }

    /// A network in use, by a lease or a pairing, keeps its leases; one in no use has them
    /// removed.
    #[test]
    fn a_network_in_use_keeps_its_leases() {
        let (home, held, leases) = network("leases-clear");
        let range = pool("10.89.0.0/24");
        let a = leases.take(&held, &range, None, &id('a')).unwrap();
        assert_eq!(leases.clear(&held).unwrap_err(), Refused::InUse);
        drop(a);
        let pairing = leases.pair(&held).unwrap();
        assert_eq!(leases.clear(&held).unwrap_err(), Refused::InUse);
        drop(pairing);
        leases.clear(&held).unwrap();
        assert!(!home.join("networks").join(format!("{}.leases", id('e'))).exists());
        assert_eq!(Leases::of(&home, "../e").unwrap_err(), Refused::NotAnId);
    }

    /// The home's networks lock keeps every other open of it out while held, as another
    /// process's would be, and lets the next in as it drops.
    #[test]
    fn the_homes_networks_are_held_against_every_other_open() {
        let home = shards_testdir::TempDir::new("leases-hold").unwrap();
        let held = hold(&home).unwrap();
        let other = File::open(home.join("networks").join(LOCK)).unwrap();
        assert!(matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(held);
        assert!(other.try_lock().is_ok());
    }
}
