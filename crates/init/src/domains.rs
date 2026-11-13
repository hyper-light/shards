//! Agents and harnesses, each started in its domain (docs/design/architecture.md D59;
//! AGENTFILE_ARCH.md §9.3, §9.9): the image's normalized Agentfile (`/.agentfile.json`)
//! says which there are and where each lies, and each one's OSI config (`<dir>.d/osi.json`)
//! how it runs. One whose config says nothing of how it runs is files alone, for another
//! to use.
//!
//! Each starts with `clone3` into namespaces of its own (mount, PID, IPC, network, UTS,
//! cgroup) and into a cgroup of its own, `/sys/fs/cgroup/domains/<kind>-<name>`, whose
//! `pids.max` bounds what it starts. Its first process, PID 1 of its namespace:
//!
//! - sees the image's system as built, read-only, `nosuid` and `nodev` (workloads inherit
//!   the microVM's OS), and nothing the run made of it since: none of its files, mounts or
//!   sockets, a Unix socket's path being reachable to any process that can see it
//!   (unix(7)); its own directory and grants with it; every other domain's directory
//!   and grants hidden under an empty tmpfs no one may read; `/sys` hidden; a `/proc` of
//!   its PID namespace; a `/dev` of six nodes, the `fd` and stdio links, a `shm` of its
//!   own and an `mqueue` of its IPC namespace's; and a scratch tmpfs of its own at
//!   `/tmp`, lost when it ends;
//! - beneath each of the host's path roots its run names (D235, `/Users`), what is plugged
//!   into it as it runs, at its host path, and nothing before;
//! - has only its own loopback, brought up;
//! - runs as uid and gid `200000 + n`, the n-th domain's, with no supplementary groups and
//!   no capability left, bounding set included, and `no_new_privs` set;
//! - has its signals as a new process has them, none ignored and none blocked, as an
//!   exec's do: init's SIGPIPE, which Rust's runtime ignores, would stay ignored past
//!   execve;
//! - holds no descriptor of init's but its stdio: stdin `/dev/null`, or where its owner
//!   asked for its input as the run was made a pipe whose write end init holds
//!   ([`crate::input`]), stdout and stderr pipes of their own that init relays to its
//!   owner as managed records ([`crate::capture`]);
//! - runs the command its config says, a relative program its directory's, a bare name
//!   found on `PATH` before it starts.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::json::{self, Value};

mod admission;

/// The first domain's uid and gid; the n-th's is this plus n.
pub const FIRST_ID: u32 = shards_abi::DOMAIN_FIRST_ID;
/// Where the domains' cgroups are.
const CGROUPS: &str = "/sys/fs/cgroup/run/domains";
/// The `PATH` a domain starts with: Docker's default (moby oci/defaults.go).
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// The nodes a domain's `/dev` holds, as Docker's containers' (moby oci/defaults.go,
/// `DefaultLinuxDevices` less `/dev/console`): name, major, minor.
const DEV: [(&CStr, u32, u32); 6] = [
    (c"/dev/null", 1, 3),
    (c"/dev/zero", 1, 5),
    (c"/dev/full", 1, 7),
    (c"/dev/random", 1, 8),
    (c"/dev/urandom", 1, 9),
    (c"/dev/tty", 5, 0),
];
/// Its links, as Docker's (runc libcontainer/rootfs_linux.go, `setupDevSymlinks`).
const LINKS: [(&CStr, &CStr); 4] = [
    (c"/proc/self/fd", c"/dev/fd"),
    (c"/proc/self/fd/0", c"/dev/stdin"),
    (c"/proc/self/fd/1", c"/dev/stdout"),
    (c"/proc/self/fd/2", c"/dev/stderr"),
];

/// A domain to start.
#[derive(Debug, Clone)]
pub struct Domain {
    /// The trusted display label, independent of its numeric tenant identity.
    pub label: String,
    /// Its cgroup's name, `agent-main`, which is its host name too.
    pub cgroup: String,
    pub dir: Vec<u8>,
    pub id: u32,
    /// `pids.max`: `None` for the microVM's own.
    pub pids: Option<u64>,
    /// `--processes=none`: it starts threads alone (§9.9).
    pub no_processes: bool,
    /// `memory.max`, in bytes, where its config asks one (`asks.memory`): under the
    /// domains' own, which every domain has.
    pub memory: Option<u64>,
    /// Its config's `run.command`, as written: its program is found as it starts
    /// ([`executable`]), on the `PATH` it runs with.
    pub argv: Vec<CString>,
    /// Its config's `run.env`, as written: its environment is made as it starts
    /// ([`harness_env`]), with the image's and what is staged for it.
    pub run_env: Vec<String>,
    pub workdir: CString,
    /// Whether it is a harness, and its name, as `CONNECT`s name it.
    pub name: crate::netplan::Name,
    /// Its link to the others, where the Agentfile grants it one.
    pub link: Option<crate::netplan::Link>,
    /// Those it may message through the in-VM server (D60), by index among the domains
    /// that run: each with whether it may send them requests, and whether it answers
    /// theirs.
    pub channels: Vec<(usize, bool, bool)>,
    /// The MCP servers offered it (§4.4, §12 answer 5), as its server instance lists
    /// them: a JSON array.
    pub mcp: String,
    /// Its `/etc/resolv.conf`, where it reaches past the microVM: the microVM's gateway,
    /// whose network process asks the host's resolvers (D59).
    pub resolv: Option<Vec<u8>>,
    /// What the Agentfile's `VOLUME`s say of their mount points for it (D109).
    pub volumes: Vec<VolumeRule>,
}

/// What an Agentfile `VOLUME` says of one of its mount points for a domain (AGENTFILE_ARCH.md
/// §4.5, §12 answers 8 and 9): whether the domain is given the volume there (every domain
/// where no `FOR` scopes it), and its owner and mode (`--chown`, `--chmod`), as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRule {
    pub path: String,
    pub granted: bool,
    pub chown: String,
    pub chmod: String,
}

/// A volume the run gives its domains (D109), as the host's setup entry says it
/// (`volume=`, or `domain-volume=` for one the run's own root does not mount, each with
/// the `domains` flag): its virtio-fs tag, its mount point, whether it is read-only,
/// whether the image's files there are copied into it where it is empty, and whether the
/// run's own root mounts it, which copies them then.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Share {
    tag: String,
    dest: String,
    ro: bool,
    copy: bool,
    main: bool,
    /// The store's own volume, which `--chown` and `--chmod` set: never a host directory.
    own: bool,
}

/// The shares the run gives its domains, from its setup entries.
fn shares_of(setup: &[Vec<u8>]) -> Vec<Share> {
    let mut out = Vec::new();
    for e in setup {
        let (rest, main) = if let Some(r) = e.strip_prefix(b"volume=") {
            (r, true)
        } else if let Some(r) = e.strip_prefix(b"domain-volume=") {
            (r, false)
        } else {
            continue;
        };
        let parts: Vec<&[u8]> = rest.split(|&b| b == 0).collect();
        let [tag, dest, flags, only] = parts.as_slice() else {
            continue;
        };
        let flags: Vec<&[u8]> = flags.split(|&b| b == b',').collect();
        if !only.is_empty() || !flags.contains(&&b"domains"[..]) {
            continue;
        }
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        out.push(Share {
            tag: text(tag),
            dest: crate::copy::clean(std::path::Path::new(&text(dest)))
                .to_string_lossy()
                .into_owned(),
            ro: flags.contains(&&b"ro"[..]),
            copy: flags.contains(&&b"copy"[..]),
            main,
            own: flags.contains(&&b"own"[..]),
        });
    }
    out
}

/// The image as built, its layers read-only (`changes::lower`), every path resolved in
/// it: `None` where they were not kept.
fn as_built() -> Result<Option<crate::inroot::Root>, String> {
    let Some(lower) = crate::changes::lower() else {
        return Ok(None);
    };
    crate::inroot::Root::open(std::path::Path::new(&format!(
        "/proc/self/fd/{}",
        lower.as_raw_fd()
    )))
    .map(Some)
    .map_err(|e| format!("the image's layers: {e}"))
}

/// The file at `path` in the image as built, its symlinks resolved there, read to at
/// most one byte past what [`json::parse`] takes; `None` where there is none.
fn built_file(root: &crate::inroot::Root, path: &str) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read as _;
    let file = match root.open_at(path.as_bytes(), libc::O_RDONLY) {
        Ok(fd) => std::fs::File::from(fd),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{path}: {e}")),
    };
    let mut data = Vec::new();
    file.take(json::MAX_LEN as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("{path}: {e}"))?;
    Ok(Some(data))
}

/// One volume at each mount point, as the build makes them: a domain is given what is
/// mounted where its rules say, so a second volume there would go to the first's domains.
fn once_each(paths: impl Iterator<Item = String>) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for p in paths {
        if !seen.insert(crate::copy::clean(std::path::Path::new(&p))) {
            return Err(format!("/.agentfile.json: two volumes mount {p}"));
        }
    }
    Ok(())
}

/// Whether `path` is `dir` or beneath it.
fn within(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// What init mounts for each domain that a volume's mount point lies in (D109). The build
/// and the daemon refuse the same (`shards_agentfile::agentfile::DomainMount`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomainMount {
    /// Its root, or `/proc`, `/sys` or `/dev`, the file systems mounted there.
    System,
    /// `/tmp`, its scratch (D139). The scratch hides a mount point beneath it, and a volume
    /// at it hides the scratch, where its server reads the files it hands across and what
    /// is staged for it is written (D231).
    Scratch,
}

impl std::fmt::Display for DomainMount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DomainMount::System => "a volume may not cover a domain's root, /proc, /sys or /dev",
            DomainMount::Scratch => "a volume may not be at or under /tmp, a domain's own scratch",
        })
    }
}

/// The [`DomainMount`] `path`, absolute and clean, lies in, if any.
fn domain_mount(path: &str) -> Option<DomainMount> {
    if path == "/" || ["/proc", "/sys", "/dev"].iter().any(|s| within(path, s)) {
        Some(DomainMount::System)
    } else if within(path, "/tmp") {
        Some(DomainMount::Scratch)
    } else {
        None
    }
}

/// Whether the domains are given the volume a run mounts at `dest`, which lies at `real`
/// in the image: not where either lies in a [`DomainMount`]. One the Agentfile declares
/// there (`declared`), or whose `dest` lies there, is refused, as the build and the daemon
/// refuse it; a Docker `VOLUME` an image's symlink leads there stays the run's own root's.
fn given_at(dest: &str, real: &str, declared: bool) -> Result<bool, DomainMount> {
    match (domain_mount(dest), domain_mount(real)) {
        (Some(m), _) => Err(m),
        (None, Some(m)) if declared => Err(m),
        (None, Some(_)) => Ok(false),
        (None, None) => Ok(true),
    }
}

/// Whether `d` is given the volume at `dest`: where a `VOLUME` names it, as that says;
/// a volume no `VOLUME` scopes (Docker's own, or one with no `FOR`), every domain.
fn given_volume(d: &Domain, dest: &str) -> bool {
    let rules: Vec<&VolumeRule> = d.volumes.iter().filter(|r| r.path == dest).collect();
    rules.is_empty() || rules.iter().any(|r| r.granted)
}

/// The owner `--chown` names (§12 answer 9): an agent or harness by its name, its own
/// uid and gid; else a user and perhaps a group, by name or ID, as the image's own
/// databases say them.
fn owner(
    chown: &str,
    domains: &[Domain],
    passwd: Option<&[u8]>,
    group: Option<&[u8]>,
) -> Result<(u32, u32), String> {
    if let Some(d) = domains.iter().find(|d| d.name.1 == chown) {
        return Ok((d.id, d.id));
    }
    let u = shards_user::resolve(chown.as_bytes(), passwd, group)?;
    Ok((u.uid, u.gid))
}

/// Why init refuses a run its nesting grant, of what the image as built declares (D234).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nesting {
    /// Its normalized Agentfile declares no `NESTING`, or it has none.
    Undeclared,
    /// It declares the grant beside an agent or harness, `kind` and `name`: the grant
    /// makes the command its microVM's only principal.
    Beside { kind: &'static str, name: String },
    /// Its normalized Agentfile, as built, could not be read.
    Unread(String),
}

impl std::fmt::Display for Nesting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Nesting::Undeclared => f.write_str(&shards_abi::run::nesting::undeclared()),
            Nesting::Beside { kind, name } => f.write_str(&shards_abi::run::beside::nesting(kind, name)),
            Nesting::Unread(why) => write!(
                f,
                "cannot give the command the nesting grant: its image's Agentfile, as built: {why}"
            ),
        }
    }
}

/// Init's own check of a run carrying the nesting grant (D234), against the image as
/// built, never the run's root, which the run and `cp` write (D109): its normalized
/// Agentfile must declare `NESTING`, and no agent or harness beside it.
pub fn nesting() -> Result<(), Nesting> {
    let Some(root) = as_built().map_err(Nesting::Unread)? else {
        return Err(match std::fs::symlink_metadata("/.agentfile.json") {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Nesting::Undeclared,
            _ => Nesting::Unread("its layers were not kept".into()),
        });
    };
    let Some(text) = built_file(&root, "/.agentfile.json").map_err(Nesting::Unread)? else {
        return Err(Nesting::Undeclared);
    };
    nesting_of(&text)
}

/// What `text`, an image's normalized Agentfile, declares of the grant: `NESTING`, a
/// top-level `"nesting": true` (written only where declared, so that every Agentfile
/// without it reads as before), and no agent or harness beside it.
fn nesting_of(text: &[u8]) -> Result<(), Nesting> {
    let spec = json::parse(text).map_err(|e| Nesting::Unread(format!("/.agentfile.json: {e}")))?;
    if spec.get("schemaVersion").and_then(Value::u64) != Some(1) {
        return Err(Nesting::Unread(
            "/.agentfile.json: a schema version this init does not know".into(),
        ));
    }
    if spec.get("nesting") != Some(&Value::Bool(true)) {
        return Err(Nesting::Undeclared);
    }
    for (kind, list) in [("agent", "agents"), ("harness", "harnesses")] {
        if let Some(d) = spec.get(list).map(Value::array).and_then(<[Value]>::first) {
            let name = d.get("name").and_then(Value::str).unwrap_or_default();
            return Err(Nesting::Beside {
                kind,
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// What the image's normalized Agentfile says to start: every domain's directory, the
/// domains that run, and the pairs of them that may open connections, by index.
pub type Read = (
    Vec<Vec<u8>>,
    Vec<Domain>,
    Vec<(usize, usize, Vec<crate::netplan::Egress>)>,
);

/// The directories of every domain the image's normalized Agentfile declares, and those
/// domains that say how they run; nothing where the image has no Agentfile.
pub fn read() -> Result<Read, String> {
    let none = || Ok((Vec::new(), Vec::new(), Vec::new()));
    // The image as built, never the run's root, which the run and `cp` write (D109): what
    // it declares is what the daemon read and checked against the image.
    let Some(root) = as_built()? else {
        return match std::fs::symlink_metadata("/.agentfile.json") {
            Err(e) if e.kind() == io::ErrorKind::NotFound => none(),
            _ => Err("the image's layers were not kept, where its Agentfile is read".into()),
        };
    };
    let Some(text) = built_file(&root, "/.agentfile.json")? else {
        return none();
    };
    let spec = json::parse(&text).map_err(|e| format!("/.agentfile.json: {e}"))?;
    if spec.get("schemaVersion").and_then(Value::u64) != Some(1) {
        return Err("/.agentfile.json: a schema version this init does not know".into());
    }
    let mut dirs = Vec::new();
    let mut out = Vec::new();
    let mut n = 0u32;
    for (kind, list) in [("agent", "agents"), ("harness", "harnesses")] {
        for d in spec.get(list).map(Value::array).unwrap_or_default() {
            let name = d
                .get("name")
                .and_then(Value::str)
                .ok_or("a domain without a name")?;
            let dir = d
                .get("to")
                .and_then(Value::str)
                .ok_or("a domain without its directory")?;
            if !dir.starts_with('/') {
                return Err(format!("{kind} {name}: its directory {dir:?} is not absolute"));
            }
            dirs.push(dir.as_bytes().to_vec());
            let id = FIRST_ID.checked_add(n).ok_or("more domains than uids")?;
            n += 1;
            let path = format!("{dir}.d/osi.json");
            let config = match built_file(&root, &path)? {
                Some(c) => json::parse(&c).map_err(|e| format!("{path}: {e}"))?,
                None => continue,
            };
            let Some(run) = config.get("run") else { continue };
            out.push(domain(kind, name, dir, id, d, &config, run)?);
        }
    }
    // Their links, clear of the microVM's own network.
    let names: Vec<crate::netplan::Name> = out.iter().map(|d| d.name.clone()).collect();
    let own = crate::net::current().map(|(addr, prefix, gateway)| {
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        (std::net::Ipv4Addr::from(u32::from(addr) & mask), prefix, gateway)
    });
    let own6 = crate::net::current6().map(|(addr, prefix, _)| {
        let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
        (std::net::Ipv6Addr::from(u128::from(addr) & mask), prefix)
    });
    let plan = crate::netplan::plan(&spec, &names, own, own6)?;
    for d in &mut out {
        d.mcp = offered(&spec, d, &root);
    }
    for (x, y) in crate::netplan::channels(&spec, &names) {
        for (me, peer, send) in [(x, y, true), (y, x, false)] {
            if let Some(d) = out.get_mut(me) {
                match d.channels.iter_mut().find(|c| c.0 == peer) {
                    Some(c) if send => c.1 = true,
                    Some(c) => c.2 = true,
                    None => d.channels.push((peer, send, !send)),
                }
            }
        }
    }
    let volumes = spec.get("volumes").map(Value::array).unwrap_or_default();
    once_each(
        volumes
            .iter()
            .flat_map(|v| v.get("paths").and_then(Value::strings).unwrap_or_default()),
    )?;
    for v in volumes {
        let paths = v.get("paths").and_then(Value::strings).unwrap_or_default();
        let scope = v.get("for");
        let kind = scope.and_then(|f| f.get("kind")).and_then(Value::str);
        let names = scope
            .and_then(|f| f.get("names"))
            .and_then(Value::strings)
            .unwrap_or_default();
        let text = |k: &str| v.get(k).and_then(Value::str).unwrap_or_default().to_string();
        for d in &mut out {
            let (harness, me) = (d.name.0, &d.name.1);
            let granted = names.is_empty()
                || (names.iter().any(|n| n == me)
                    && match kind {
                        Some("agent") => !harness,
                        Some("harness") => harness,
                        _ => true,
                    });
            for p in &paths {
                d.volumes.push(VolumeRule {
                    path: crate::copy::clean(std::path::Path::new(p))
                        .to_string_lossy()
                        .into_owned(),
                    granted,
                    chown: text("chown"),
                    chmod: text("chmod"),
                });
            }
        }
    }
    for (d, link) in out.iter_mut().zip(plan.links) {
        // A resolver only where a grant names one (`--dns`, a remote MCP server): the
        // agents' resolver, at the switch's end of its gate's link (D61), which holds it to
        // what it may ask.
        if let Some(l) = &link
            && (l.dns || !l.named.is_empty())
            && !l.addresses.is_empty()
        {
            d.resolv =
                Some(format!("nameserver {}\noptions ndots:0\n", crate::links::TRANSIT_SWITCH).into_bytes());
        }
        d.link = link;
    }
    Ok((dirs, out, plan.pairs))
}

/// The MCP servers the Agentfile offers `d` (§4.4, §12 answer 5): those with no `FOR`,
/// to every agent, and those whose `FOR` names it. A remote one by its URL; one spoken to
/// over stdio by where it lies in `d`'s view, `/mcp/<name>`, or `<its dir>.d/mcp/<name>`
/// where `FOR` names it, and the command its OSI config gives (`<that>.d/osi.json`),
/// its program made absolute there. A JSON array.
fn offered(spec: &Value, d: &Domain, root: &crate::inroot::Root) -> String {
    let (harness, me) = (&d.name.0, &d.name.1);
    let mut items = Vec::new();
    for m in spec.get("mcp").map(Value::array).unwrap_or_default() {
        let Some(name) = m.get("name").and_then(Value::str) else {
            continue;
        };
        let Some(source) = m.get("source").and_then(Value::str) else {
            continue;
        };
        let scope = m.get("for");
        let kind = scope.and_then(|f| f.get("kind")).and_then(Value::str);
        let names = scope
            .and_then(|f| f.get("names"))
            .and_then(Value::strings)
            .unwrap_or_default();
        let ours = if names.is_empty() {
            !harness
        } else {
            names.iter().any(|n| n == me)
                && match kind {
                    Some("agent") => !harness,
                    Some("harness") => *harness,
                    _ => true,
                }
        };
        if !ours {
            continue;
        }
        if source.starts_with("https://") || source.starts_with("http://") {
            items.push(format!(
                r#"{{"name":{},"remote":true,"url":{}}}"#,
                quote(name),
                quote(source)
            ));
            continue;
        }
        let dir = if names.is_empty() {
            format!("/mcp/{name}")
        } else {
            format!("{}.d/mcp/{name}", String::from_utf8_lossy(&d.dir))
        };
        let command: Vec<String> = built_file(root, &format!("{dir}.d/osi.json"))
            .ok()
            .flatten()
            .and_then(|c| json::parse(&c).ok())
            .and_then(|c| {
                c.get("run")
                    .and_then(|r| r.get("command"))
                    .and_then(Value::strings)
            })
            .map(|mut c| {
                if let Some(first) = c.first_mut()
                    && !first.starts_with('/')
                    && first.contains('/')
                {
                    *first = format!("{dir}/{first}");
                }
                c
            })
            .unwrap_or_default();
        let command: Vec<String> = command.iter().map(|c| quote(c)).collect();
        items.push(format!(
            r#"{{"name":{},"remote":false,"dir":{},"command":[{}]}}"#,
            quote(name),
            quote(&dir),
            command.join(",")
        ));
    }
    format!("[{}]", items.join(","))
}

/// `s` as a JSON string (RFC 8259 §7).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A supplied domain's environment, each name once and the last given winning (kiln plan
/// 01a1cd8e862e7169a074db3d5fa33f3d, step 7; a program reads a name's first entry):
/// - init's own: `PATH`, the domain's directory and its `bin` first, then the image's
///   `PATH`, else Docker's; `HOME=/tmp`, its scratch; and `SHARDS_DOMAIN`, its own name;
/// - over them, the image's environment (`image`, its config's `Env`), as Docker lays an
///   image's over its own defaults (moby daemon/container/env.go, ReplaceOrAppendEnvValues),
///   but for its `PATH`, which init's holds, and its `HOME`: one for every member could be
///   private to none, and a domain's is its own;
/// - over those, its config's `run.env` (`given`), its own, which no other member shares.
///
/// What its orchestrator stages goes over all of it (staged.rs `env`, D231). `SHARDS_DOMAIN`
/// is init's alone: a domain's own name, never its image's word.
fn harness_env(dir: &str, label: &str, image: &[String], given: &[String]) -> Result<Vec<String>, String> {
    let base = image
        .iter()
        .rev()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or(PATH);
    let mut env = vec![
        format!("PATH={dir}:{dir}/bin:{base}"),
        "HOME=/tmp".to_string(),
        format!("SHARDS_DOMAIN={label}"),
    ];
    let name = |e: &str, whose: &str| -> Result<String, String> {
        let Some((name, _)) = e.split_once('=').filter(|(name, _)| !name.is_empty()) else {
            return Err(format!("{label}: {whose} entry {e:?} is no NAME=value"));
        };
        if name == "SHARDS_DOMAIN" {
            return Err(format!(
                "{label}: {whose} sets SHARDS_DOMAIN, which is init's: a domain's own name"
            ));
        }
        Ok(name.to_string())
    };
    let mut put = |e: &String, name: &str| {
        let at = env
            .iter()
            .position(|kept| kept.split_once('=').is_some_and(|(n, _)| n == name));
        match at.and_then(|i| env.get_mut(i)) {
            Some(kept) => kept.clone_from(e),
            None => env.push(e.clone()),
        }
    };
    for e in image {
        let name = name(e, "the image's ENV")?;
        if !matches!(name.as_str(), "PATH" | "HOME") {
            put(e, &name);
        }
    }
    for e in given {
        put(e, &name(e, "run.env")?);
    }
    Ok(env)
}

/// The program `d` starts, its config's `run.command`'s first word: an absolute one as
/// written, a relative one its directory's, and a bare name found on the `PATH` of `env`,
/// the environment it runs with, as execvp(3) finds one (its own artifact's before the
/// system's tools, the inherited `PATH` letting a launcher such as nohup find its child):
/// here, as the child may not allocate.
fn executable(d: &Domain, env: &[CString]) -> Result<CString, String> {
    let first = d.argv.first().map(|a| a.to_bytes()).unwrap_or_default();
    let cstr = |b: Vec<u8>| CString::new(b).map_err(|_| format!("{}: a NUL in its program", d.label));
    if first.starts_with(b"/") {
        return cstr(first.to_vec());
    }
    if first.contains(&b'/') {
        return cstr([d.dir.as_slice(), b"/", first].concat());
    }
    let path = env
        .iter()
        .rev()
        .find_map(|e| e.to_bytes().strip_prefix(b"PATH="))
        .unwrap_or_default();
    path.split(|&b| b == b':')
        .filter_map(|p| CString::new([p, b"/", first].concat()).ok())
        // SAFETY: access(2) of a NUL-terminated path.
        .find(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 })
        .ok_or_else(|| {
            format!(
                "{}: {:?} is on no directory of PATH",
                d.label,
                String::from_utf8_lossy(first)
            )
        })
}

/// `d`'s command line (D256): its program as found ([`executable`]), the rest of its config's
/// `run.command` (its entrypoint's words, then its options), then the run-time arguments its
/// run stages, in their order; none staged, its config's alone.
fn command_line<'a>(program: &'a CStr, d: &'a Domain, args: &'a [CString]) -> Vec<&'a CStr> {
    std::iter::once(program)
        .chain(d.argv.iter().skip(1).map(CString::as_c_str))
        .chain(args.iter().map(CString::as_c_str))
        .collect()
}

/// Where `home`, a member's `HOME`, lies among the mounts it may write as it starts, each by
/// its mount point in its view (`points`: its scratch's `/tmp`, a writable volume's as
/// declared and as it lies in the image): the deepest that is `home` or holds it, by its
/// place in `points`, and `home`'s components beneath it. None where none does, or where
/// `home` is no absolute path that stays where it says (`..`).
fn home_in<'h>(home: &'h str, points: &[&str]) -> Option<(usize, Vec<&'h str>)> {
    let comps: Vec<&str> = home
        .strip_prefix('/')?
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if comps.contains(&"..") {
        return None;
    }
    let plain = format!("/{}", comps.join("/"));
    let (at, point) = points
        .iter()
        .enumerate()
        .filter(|(_, p)| within(&plain, p))
        .max_by_key(|(_, p)| p.len())?;
    let depth = point.split('/').filter(|c| !c.is_empty()).count();
    Some((at, comps.get(depth..).unwrap_or_default().to_vec()))
}

/// Whether `p` is relative and stays inside the directory it is relative to: the twin of
/// the build's (shards_image osi.rs `inside`), as init links no host crate.
fn inside(p: &str) -> bool {
    if p.is_empty() || p.starts_with('/') || p.contains('\0') {
        return false;
    }
    let mut depth = 0u32;
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => depth = depth.saturating_add(1),
        }
    }
    true
}

fn domain(
    kind: &str,
    name: &str,
    dir: &str,
    id: u32,
    declared: &Value,
    config: &Value,
    run: &Value,
) -> Result<Domain, String> {
    let label = format!("{kind} {name}");
    let cstr = |s: &str| CString::new(s).map_err(|_| format!("{label}: a NUL in {s:?}"));
    let command = run
        .get("command")
        .and_then(Value::strings)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| format!("{label}: run.command is no list of strings"))?;
    // `--processes` as declared, else what its config asks, else the microVM's. `none`
    // is its seccomp filter's: pids.max counts threads, which it may start.
    let no_processes = matches!(declared.get("processes"), Some(Value::String(s)) if s == "none");
    let pids = match declared.get("processes") {
        Some(Value::String(s)) if s == "none" => None,
        Some(Value::Null) | None => config
            .get("asks")
            .and_then(|a| a.get("processes"))
            .and_then(Value::u64),
        Some(v) => Some(v.u64().ok_or_else(|| format!("{label}: processes is no count"))?),
    };
    let memory = match config.get("asks").and_then(|a| a.get("memory")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.u64()
                .ok_or_else(|| format!("{label}: asks.memory is no count of bytes"))?,
        ),
    };
    // Its own working directory, in its directory, as its build checks it (shards_image
    // osi.rs `Config::check`): a run may name another (D231, `start`).
    let workdir = run.get("workdir").and_then(Value::str).unwrap_or(".");
    if !inside(workdir) {
        return Err(format!(
            "{label}: run.workdir {workdir:?} must be relative and stay inside its directory"
        ));
    }
    let argv = command.iter().map(|a| cstr(a)).collect::<Result<Vec<_>, _>>()?;
    let workdir = cstr(&format!("{dir}/{workdir}"))?;
    Ok(Domain {
        cgroup: format!("{kind}-{name}"),
        label,
        dir: dir.as_bytes().to_vec(),
        id,
        pids,
        no_processes,
        memory,
        argv,
        run_env: run.get("env").and_then(Value::strings).unwrap_or_default(),
        workdir,
        name: (kind == "harness", name.to_string()),
        link: None,
        channels: Vec::new(),
        mcp: "[]".into(),
        resolv: None,
        volumes: Vec::new(),
    })
}

/// A started tenant: stable declared identity, owned lifetime and private output.
#[derive(Debug)]
pub struct Started {
    pub member: u32,
    pub output_control: crate::capture::OutputControl,
    pub label: String,
    /// Its cgroup's directory.
    pub cgroup: String,
    pub lifecycle: crate::managed::Owned,
    pub stdout: crate::capture::Pipe,
    pub stderr: crate::capture::Pipe,
    /// Its stdin, where its owner asked for it: else `/dev/null`.
    pub input: Option<crate::input::Input>,
    /// Its end has been told its owner (`MEMBER_ENDED`, D231).
    pub end_told: bool,
    /// What confines its processes, kept for a command run in it ([`crate::enter`]).
    pub confinement: Confinement,
    /// A descriptor of its own terminal's master, where its stdio is one (plan step 6,
    /// D244), kept to size it (`builtin::RESIZE`).
    pub terminal: Option<OwnedFd>,
}

impl Started {
    pub fn abort_output(&mut self, errno: i32) {
        self.stdout.abort();
        self.stderr.abort();
        if matches!(self.output_control, crate::capture::OutputControl::Active) {
            self.output_control = crate::capture::OutputControl::Pending(errno);
        }
    }
}

/// `struct clone_args` (include/uapi/linux/sched.h), as `clone3` reads it.
#[repr(C)]
#[derive(Default)]
pub(crate) struct CloneArgs {
    pub(crate) flags: u64,
    pub(crate) pidfd: u64,
    pub(crate) child_tid: u64,
    pub(crate) parent_tid: u64,
    pub(crate) exit_signal: u64,
    pub(crate) stack: u64,
    pub(crate) stack_size: u64,
    pub(crate) tls: u64,
    pub(crate) set_tid: u64,
    pub(crate) set_tid_size: u64,
    pub(crate) cgroup: u64,
}

/// include/uapi/linux/sched.h.
pub(crate) const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// Landlock (include/uapi/linux/landlock.h; Documentation/userspace-api/landlock.rst).
mod landlock {
    /// `landlock_create_ruleset`'s flag that asks the ABI version.
    pub const CREATE_RULESET_VERSION: u32 = 1;
    pub const RULE_PATH_BENEATH: u32 = 1;
    pub const EXECUTE: u64 = 1 << 0;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const READ_FILE: u64 = 1 << 2;
    pub const READ_DIR: u64 = 1 << 3;
    pub const REMOVE_FILE: u64 = 1 << 5;
    pub const MAKE_SOCK: u64 = 1 << 9;
    /// `IOCTL_DEV`, ABI 5: the last filesystem right, so every right is below it.
    pub const IOCTL_DEV: u64 = 1 << 15;
    pub const FS_ALL: u64 = (IOCTL_DEV << 1) - 1;
    /// ABI 4.
    pub const NET_BIND_TCP: u64 = 1 << 0;
    pub const NET_CONNECT_TCP: u64 = 1 << 1;
    /// ABI 6.
    pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    pub const SCOPE_SIGNAL: u64 = 1 << 1;
    /// The first ABI with every right used here: IOCTL_DEV is 5, the scopes 6.
    pub const NEEDED: i64 = 6;

    #[repr(C)]
    pub struct RulesetAttr {
        pub handled_access_fs: u64,
        pub handled_access_net: u64,
        pub scoped: u64,
    }

    #[repr(C, packed)]
    pub struct PathBeneathAttr {
        pub allowed_access: u64,
        pub parent_fd: i32,
    }
}

/// The attributes of the microVM's system as a domain sees it, its root's first (D59):
/// read-only, `nosuid` and `nodev`. Read-only is what keeps a plug root (D235) granting
/// nothing until something is plugged there, as Landlock grants every right beneath one.
const SYSTEM: u64 = libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV;

/// Everything a domain's first process uses, made before it is: after `clone3` it calls
/// the kernel alone.
struct Prepared {
    hide: Vec<CString>,
    hostname: CString,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    /// Its `/etc/hosts`, where it has a link.
    hosts: Option<Vec<u8>>,
    /// Its `/etc/resolv.conf`, where it reaches past the microVM.
    resolv: Option<Vec<u8>>,
    /// Where its image's trust bundle lies, and the bundle with the run's CA laid over it
    /// (D242), where its link reaches a host the run's credential grants name.
    trust: Option<(CString, Vec<u8>)>,
    /// Where it waits for its link, before it holds nothing of init's.
    go: Option<RawFd>,
    /// Where it sends the listener of the chooser's filter, and the filter, for a domain
    /// among several given a port (D122, crate::chooser).
    chooser: Option<(RawFd, *const libc::sock_fprog)>,
    /// Its stdin: the read end of its input's pipe, where its owner asked for its input
    /// ([`crate::input`]); else `/dev/null`, opened as it starts.
    stdin: Option<RawFd>,
    /// Its stdio is a terminal of its own (plan step 6, D244): the descriptor its stdout is
    /// given is the terminal's other end, which is its stdin and stderr too and its
    /// session's controlling terminal.
    terminal: bool,
    /// Its root: the image as built, a mount not yet attached.
    root: RawFd,
    /// Its Unix sockets' directories, each a mount not yet attached and where it goes,
    /// `/run/networks/<network>/<name>`; read-only where it only connects.
    sockets: Vec<(RawFd, CString)>,
    /// Its volumes (D109), each a mount not yet attached, and its mount point.
    volumes: Vec<(RawFd, CString)>,
    /// Its scratch, `/tmp`, a mount not yet attached, where the image has a `/tmp` (D139).
    scratch: Option<RawFd>,
    /// Its /dev/shm's options ([`shm_of`]).
    shm: CString,
}

/// The options of a domain's /dev/shm: its own, mode 0700, as large as its run's (the run's
/// `shm=` setup entry), else as the microVM's (D156: every /dev/shm of a microVM half the
/// memory it may have, where a browser's renderers and a data loader's workers share
/// memory). It was Docker's 64 MiB, which those outgrow.
fn shm_of(setup: &[Vec<u8>]) -> Result<CString, String> {
    let options = match setup.iter().find_map(|e| e.strip_prefix(b"shm=")) {
        Some(size) => format!("mode=700,size={}", String::from_utf8_lossy(size)),
        None => crate::run::shm().replacen("mode=1777", "mode=700", 1),
    };
    CString::new(options).map_err(|_| "a domain's /dev/shm: a NUL in its options".to_string())
}

/// `open_tree`'s and `mount_setattr`'s (include/uapi/linux/mount.h).
mod tree {
    pub const OPEN_TREE_CLONE: u32 = 1;
    pub const OPEN_TREE_CLOEXEC: u32 = libc::O_CLOEXEC as u32;
}

/// A copy of the mount of directory `dir`, attached nowhere, with `attr` set: a mount of
/// init's own, as `open_tree` has copied since Linux 5.2, and `mount_setattr` changes since
/// 5.12.
pub(crate) fn copy_of(dir: &str, attr: u64) -> io::Result<OwnedFd> {
    let path = CString::new(dir).map_err(io::Error::other)?;
    // SAFETY: open_tree(2) of a NUL-terminated path.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            path.as_ptr(),
            tree::OPEN_TREE_CLONE | tree::OPEN_TREE_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor open_tree just made, owned here alone.
    let copy = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    let set: [u64; 4] = [attr, 0, 0, 0];
    // SAFETY: mount_setattr(2) of the copy just made, with a struct mount_attr it owns.
    if unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            copy.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            set.as_ptr(),
            std::mem::size_of_val(&set),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(copy)
}

/// The new mount API's (include/uapi/linux/mount.h).
mod mount_api {
    pub const FSOPEN_CLOEXEC: u32 = 0x1;
    pub const FSCONFIG_SET_STRING: u32 = 1;
    pub const FSCONFIG_CMD_CREATE: u32 = 6;
    pub const FSMOUNT_CLOEXEC: u32 = 0x1;
    pub const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;
}

/// A new mount of `fstype` with `options`, attached nowhere.
pub(crate) fn detached(fstype: &CStr, options: &[(&CStr, &CStr)]) -> io::Result<OwnedFd> {
    detached_with(fstype, options, 0)
}

/// A new mount of `fstype` with `options` and mount attributes `attrs`
/// (`MOUNT_ATTR_*`), attached nowhere.
fn detached_with(fstype: &CStr, options: &[(&CStr, &CStr)], attrs: u64) -> io::Result<OwnedFd> {
    use mount_api::*;
    // SAFETY: fsopen(2) of a NUL-terminated name.
    let fs = unsafe { libc::syscall(libc::SYS_fsopen, fstype.as_ptr(), FSOPEN_CLOEXEC) };
    if fs < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor fsopen just made, owned here alone.
    let fs = unsafe { OwnedFd::from_raw_fd(fs as RawFd) };
    for (key, value) in options {
        // SAFETY: fsconfig(2) of NUL-terminated strings on the context just made.
        let set = unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                fs.as_raw_fd(),
                FSCONFIG_SET_STRING,
                key.as_ptr(),
                value.as_ptr(),
                0,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let null = std::ptr::null::<libc::c_char>();
    // SAFETY: fsconfig(2) and fsmount(2) on the context just made.
    let mounted = unsafe {
        if libc::syscall(
            libc::SYS_fsconfig,
            fs.as_raw_fd(),
            FSCONFIG_CMD_CREATE,
            null,
            null,
            0,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        libc::syscall(libc::SYS_fsmount, fs.as_raw_fd(), FSMOUNT_CLOEXEC, attrs)
    };
    if mounted < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor fsmount just made, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(mounted as RawFd) })
}

/// A domain's scratch, `/tmp` as it sees it, where the image has a `/tmp` (D139): a tmpfs of
/// its own, attached nowhere, `nosuid` and `nodev`, its root the domain's (uid and gid
/// `id`) and its group's to read (0750), its server instance being of that group. The
/// domain attaches it; its instance holds it, beneath which it reads the files the domain
/// hands across (AGENTFILE_ARCH §9.4).
fn scratch(lower: std::os::fd::BorrowedFd<'_>, id: u32) -> io::Result<Option<OwnedFd>> {
    // SAFETY: an all-zero stat is a valid out-parameter, which fstatat(2) fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat(2) of a NUL-terminated name beneath a descriptor of init's.
    let has = unsafe { libc::fstatat(lower.as_raw_fd(), c"tmp".as_ptr(), &raw mut st, 0) } == 0
        && st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    if !has {
        return Ok(None);
    }
    let id = CString::new(id.to_string()).map_err(io::Error::other)?;
    detached_with(
        c"tmpfs",
        &[(c"mode", c"0750"), (c"uid", &id), (c"gid", &id)],
        libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV,
    )
    .map(Some)
}

/// What a domain mounts over and the image may lack, made where it does: the run makes
/// them in its writable layer, which a domain does not see. The run's plug roots (D235,
/// `roots`) too, where the image lacks them. overlayfs shows a directory both layers hold
/// with the upper one's owner and mode (Documentation/filesystems/overlayfs.rst,
/// "Directories"), so each directory made here that the image has takes the image's, as
/// the tmpfs's root takes the image's root's: a mount point's parents (`/home/node` of a
/// volume at `/home/node/.cache`) stay as the image made them.
fn skeleton(
    lower: std::os::fd::BorrowedFd<'_>,
    sockets: &[CString],
    roots: &[CString],
) -> io::Result<OwnedFd> {
    let image = |path: &CStr| {
        // SAFETY: fstatat(2) of a NUL-terminated path into a zeroed stat it owns, a type of
        // integers alone; the empty path, with AT_EMPTY_PATH, is the image's root.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            let flags = libc::AT_SYMLINK_NOFOLLOW | if path.is_empty() { libc::AT_EMPTY_PATH } else { 0 };
            (libc::fstatat(lower.as_raw_fd(), path.as_ptr(), &raw mut st, flags) == 0).then_some(st)
        }
    };
    let is_dir = |st: &libc::stat| st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    let root = image(c"").ok_or_else(io::Error::last_os_error)?;
    let option = |n: String| CString::new(n).map_err(io::Error::other);
    let (mode, uid, gid) = (
        option(format!("{:o}", root.st_mode & 0o7777))?,
        option(root.st_uid.to_string())?,
        option(root.st_gid.to_string())?,
    );
    let skel = detached(
        c"tmpfs",
        &[
            (c"mode", mode.as_c_str()),
            (c"uid", uid.as_c_str()),
            (c"gid", gid.as_c_str()),
        ],
    )?;
    let make = |path: &CStr| -> io::Result<()> {
        // SAFETY: mkdirat(2) of a NUL-terminated path in the tmpfs just made.
        if unsafe { libc::mkdirat(skel.as_raw_fd(), path.as_ptr(), 0o755) } != 0 {
            let e = io::Error::last_os_error();
            // Made already, as an earlier mount point's parent.
            return if e.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            };
        }
        let Some(st) = image(path).filter(is_dir) else {
            return Ok(());
        };
        // SAFETY: fchownat(2) and fchmodat(2) of the directory just made in the tmpfs: its
        // owner first, as a chown clears set-ID bits (chown(2)), then its mode whole.
        unsafe {
            if libc::fchownat(
                skel.as_raw_fd(),
                path.as_ptr(),
                st.st_uid,
                st.st_gid,
                libc::AT_SYMLINK_NOFOLLOW,
            ) != 0
                || libc::fchmodat(skel.as_raw_fd(), path.as_ptr(), st.st_mode & 0o7777, 0) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    };
    for dir in [c"proc", c"dev", c"sys", c"tmp", c"etc"]
        .into_iter()
        .chain(roots.iter().map(CString::as_c_str))
    {
        if image(dir).is_none() || dir == c"etc" {
            make(dir)?;
        }
    }
    // Where its mounts go, each made with its parents: its Unix sockets' directories,
    // `run/networks/<network>/<name>`, `run` merged with the image's own; its server's; its
    // volumes'.
    for dir in sockets {
        let bytes = dir.to_bytes();
        for (at, _) in bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'/')
            .chain([(bytes.len(), &0)])
        {
            let Ok(part) = CString::new(bytes.get(..at).unwrap_or_default()) else {
                continue;
            };
            make(&part)?;
        }
    }
    for file in [c"etc/hosts", c"etc/resolv.conf"] {
        if image(file).is_some() {
            continue;
        }
        // SAFETY: openat(2) of a NUL-terminated path in the tmpfs just made.
        let fd = unsafe {
            libc::openat(
                skel.as_raw_fd(),
                file.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor openat just made, closed here.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    Ok(skel)
}

/// A domain's root: the image as built, under what [`skeleton`] adds, an overlay attached
/// nowhere. With plug roots (D235, `pluggable`) it has a writable layer of its own, a tmpfs
/// that init alone writes, through a mount of its own (hotplug.rs), to make the mount
/// points of what is plugged into the domain as it runs; without, none. The domain's own
/// mount of its root is read-only either way ([`child`]).
fn image_root(skel: &OwnedFd, lower: std::os::fd::BorrowedFd<'_>, pluggable: bool) -> io::Result<OwnedFd> {
    let layers = CString::new(format!(
        "/proc/self/fd/{}:/proc/self/fd/{}",
        skel.as_raw_fd(),
        lower.as_raw_fd()
    ))
    .map_err(io::Error::other)?;
    if !pluggable {
        // One space of inode numbers for its layers (run::XINO).
        return detached(c"overlay", &[(c"lowerdir", layers.as_c_str()), (c"xino", c"on")]);
    }
    let rw = detached(c"tmpfs", &[(c"mode", c"0755")])?;
    for dir in [c"upper", c"work"] {
        // SAFETY: mkdirat(2) of a NUL-terminated name in the tmpfs just made.
        if unsafe { libc::mkdirat(rw.as_raw_fd(), dir.as_ptr(), 0o755) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // The root's owner and mode are its upper layer's, as overlayfs shows a directory both
    // layers hold: so `upper` is given the image's own, as the domain's root is the image's.
    // SAFETY: an all-zero stat is a valid out-parameter, which fstat(2) fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of the image's root, a descriptor init holds, and fchownat(2) and
    // fchmodat(2) of a NUL-terminated name in the tmpfs just made.
    if unsafe {
        libc::fstat(lower.as_raw_fd(), &raw mut st) != 0
            || libc::fchownat(rw.as_raw_fd(), c"upper".as_ptr(), st.st_uid, st.st_gid, 0) != 0
            || libc::fchmodat(rw.as_raw_fd(), c"upper".as_ptr(), st.st_mode & 0o7777, 0) != 0
    } {
        return Err(io::Error::last_os_error());
    }
    let of =
        |dir: &str| CString::new(format!("/proc/self/fd/{}/{dir}", rw.as_raw_fd())).map_err(io::Error::other);
    let (upper, work) = (of("upper")?, of("work")?);
    detached(
        c"overlay",
        &[
            (c"lowerdir", layers.as_c_str()),
            (c"upperdir", upper.as_c_str()),
            (c"workdir", work.as_c_str()),
            (c"xino", c"on"),
        ],
    )
}

/// The domains' memory, watched: past their share (`memory.high`), the domain holding the
/// most, its scratch counted (`memory.current`), is ended whole (`cgroup.kill`). One ended
/// holds the most until it has left its cgroup, which a process does after its namespaces,
/// and with its mount namespace its scratch, are gone (kernel/exit.c, `do_exit`): so no
/// other is ended for memory it still holds.
pub struct Memory {
    events: OwnedFd,
    /// Whether each domain's end by the kernel's OOM killer was said, by its place among
    /// the domains.
    said: std::cell::RefCell<Vec<bool>>,
}

/// A domain's `memory.events` counts of the kernel's OOM handling
/// (Documentation/admin-guide/cgroup-v2.rst): `oom`, its own limit reached with nothing
/// left to reclaim, which the kernel counts before it chooses whom to kill
/// (mm/memcontrol.c, `mem_cgroup_oom`) and never for the microVM's memory as a whole; and
/// `oom_kill`, its processes the OOM killer ended, for either.
struct Ooms {
    oom: u64,
    oom_kill: u64,
}

impl Ooms {
    fn of(cgroup: &str) -> Ooms {
        let events = std::fs::read_to_string(format!("{cgroup}/memory.events")).unwrap_or_default();
        let count = |key: &str| {
            events
                .lines()
                .find_map(|l| l.split_once(' ').filter(|(k, _)| *k == key))
                .and_then(|(_, v)| v.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        Ooms {
            oom: count("oom"),
            oom_kill: count("oom_kill"),
        }
    }
}

impl Memory {
    /// Watches the domains' `memory.events`, which the kernel marks modified as they change
    /// (Documentation/admin-guide/cgroup-v2.rst, "Conventions").
    pub fn watch() -> Result<Memory, String> {
        // SAFETY: inotify_init1(2), whose descriptor is owned below.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "watching the domains' memory: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: the descriptor inotify_init1 just made, owned here alone.
        let events = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = CString::new(format!("{CGROUPS}/memory.events")).map_err(|e| e.to_string())?;
        // SAFETY: inotify_add_watch(2) of a NUL-terminated path.
        if unsafe { libc::inotify_add_watch(events.as_raw_fd(), path.as_ptr(), libc::IN_MODIFY) } < 0 {
            return Err(format!(
                "watching the domains' memory: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Memory {
            events,
            said: std::cell::RefCell::new(Vec::new()),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.events.as_raw_fd()
    }

    /// After the watch says the domains' memory changed: ends the domain holding the most
    /// where they are past their share, and says so through `say`.
    pub fn relieve(
        &self,
        domains: &mut [Started],
        mut say: impl FnMut(&[u8]),
    ) -> Result<(), crate::managed::BoundaryFault> {
        let mut buf = [0u8; 4096];
        // SAFETY: read(2) into a buffer it owns, until the watch has nothing more.
        while unsafe { libc::read(self.events.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        let number = |path: &str| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let populated = |d: &Started| {
            std::fs::read_to_string(format!("{}/cgroup.events", d.cgroup))
                .is_ok_and(|e| e.lines().any(|l| l == "populated 1"))
        };
        // A domain the kernel's OOM killer ended, whole (`memory.oom.group`), once: at its
        // own limit, where it counted an `oom`, or for the microVM's memory. Said, as
        // init's own ending of one is below.
        {
            let mut said = self.said.borrow_mut();
            said.resize(domains.len(), false);
            for (d, said) in domains.iter().zip(said.iter_mut()).filter(|(_, s)| !**s) {
                let ooms = Ooms::of(&d.cgroup);
                if ooms.oom_kill == 0 {
                    continue;
                }
                *said = true;
                let why = if ooms.oom > 0 {
                    match number(&format!("{}/memory.max", d.cgroup)) {
                        Some(max) => format!("its memory reached its limit, {max} bytes"),
                        None => "its memory reached its limit".to_string(),
                    }
                } else {
                    "the microVM's memory ran out, and the kernel's OOM killer chose it".to_string()
                };
                say(format!("[{}] shards-init: ended: {why}\n", d.label).as_bytes());
            }
        }
        let (Some(current), Some(high)) = (
            number(&format!("{CGROUPS}/memory.current")),
            number(&format!("{CGROUPS}/memory.high")),
        ) else {
            return Ok(());
        };
        if current <= high {
            return Ok(());
        }
        let most = domains
            .iter()
            .enumerate()
            .filter(|(_, d)| populated(d))
            .map(|(i, d)| (i, number(&format!("{}/memory.current", d.cgroup)).unwrap_or(0)))
            .max_by_key(|&(_, held)| held);
        let Some((i, held)) = most else { return Ok(()) };
        let Some(d) = domains.get_mut(i) else {
            return Ok(());
        };
        // What it held, as its `memory.stat` counts it, read before it is ended.
        let stat = std::fs::read_to_string(format!("{}/memory.stat", d.cgroup)).unwrap_or_default();
        let of = |key: &str| {
            stat.lines()
                .find_map(|l| l.split_once(' ').filter(|(k, _)| *k == key))
                .map_or("?", |(_, v)| v.trim())
        };
        let (anon, file, shmem, kernel) = (of("anon"), of("file"), of("shmem"), of("kernel"));
        d.lifecycle.stop()?;
        say(format!(
            "[{}] shards-init: ended: the agents' memory, {current} bytes, was past their share, {high}, and its {held} the most: anon {anon}, file {file}, shmem {shmem}, kernel {kernel}\n",
            d.label
        )
        .as_bytes());
        Ok(())
    }
}

fn cstr_of(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("a NUL in {s:?}"))
}

/// `MemAvailable` of `/proc/meminfo`, in bytes.
pub(crate) fn available_memory() -> Result<u64, String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.trim().strip_suffix("kB"))
        .and_then(|kb| kb.trim().parse::<u64>().ok())
        .and_then(|kb| kb.checked_mul(1024))
        .ok_or_else(|| "/proc/meminfo: no MemAvailable".to_string())
}

/// The Landlock ABI the guest kernel has; an error where it lacks what a domain needs.
fn landlock_abi() -> Result<i64, String> {
    // SAFETY: landlock_create_ruleset(2) asked only its version: no attribute is read.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<landlock::RulesetAttr>(),
            0usize,
            landlock::CREATE_RULESET_VERSION,
        )
    };
    if abi < landlock::NEEDED {
        let why = if abi < 0 {
            io::Error::last_os_error().to_string()
        } else {
            format!("ABI {abi}")
        };
        return Err(format!(
            "the guest kernel's Landlock ({why}) lacks ABI {}, which an agent's domain needs",
            landlock::NEEDED
        ));
    }
    Ok(abi)
}

/// A seccomp filter: its seccomp(2) flags and program.
pub type Filter = (u32, Vec<libc::sock_filter>);

/// The switch the domains' links meet in, kept while the microVM runs: a namespace no
/// process holds lives while a descriptor of it does.
pub(crate) static SWITCH: std::sync::OnceLock<crate::links::Switch> = std::sync::OnceLock::new();

/// The chooser of the ports several domains are given (D122), for the run's life.
static CHOOSER: std::sync::OnceLock<crate::chooser::Chooser> = std::sync::OnceLock::new();

/// What the `i`-th domain's gate lets through (D61): its addresses; to each peer a pair
/// lets it open flows to, at the peer's addresses, the ports granted; from each that may
/// open flows to it, likewise; its ports past the microVM; and the agents' resolver where
/// a grant gives it names.
fn gate_of(
    domains: &[Domain],
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
    i: usize,
    link: &crate::netplan::Link,
) -> crate::links::Gate {
    let addrs = |j: usize| -> Vec<std::net::Ipv4Addr> {
        domains
            .get(j)
            .and_then(|d| d.link.as_ref())
            .map(|l| l.addresses.iter().map(|a| a.addr).collect())
            .unwrap_or_default()
    };
    let addrs6 = |j: usize| -> Vec<std::net::Ipv6Addr> {
        domains
            .get(j)
            .and_then(|d| d.link.as_ref())
            .map(|l| l.addresses6.iter().map(|a| a.addr).collect())
            .unwrap_or_default()
    };
    crate::links::Gate {
        own: link.addresses.iter().map(|a| a.addr).collect(),
        opens: pairs
            .iter()
            .filter(|(from, _, _)| *from == i)
            .map(|(_, to, ports)| (addrs(*to), ports.clone()))
            .collect(),
        accepts: pairs
            .iter()
            .filter(|(_, to, _)| *to == i)
            .map(|(from, _, ports)| (addrs(*from), ports.clone()))
            .collect(),
        egress: link.egress.clone(),
        // Past the uplink's translation, at the domain's own ports (D122).
        ingress: link.ingress.iter().map(|&(_, at)| at).collect(),
        dns: link.dns || !link.named.is_empty(),
        own6: link.addresses6.iter().map(|a| a.addr).collect(),
        opens6: pairs
            .iter()
            .filter(|(from, _, _)| *from == i)
            .map(|(_, to, ports)| (addrs6(*to), ports.clone()))
            .collect(),
        accepts6: pairs
            .iter()
            .filter(|(_, to, _)| *to == i)
            .map(|(from, _, ports)| (addrs6(*from), ports.clone()))
            .collect(),
        // As the uplink carries IPv6: eth0 has IPv6, and so has this domain with egress.
        egress6: crate::net::current6().is_some() && !link.addresses6.is_empty() && !link.egress.is_empty(),
        // Every domain's networks, which its egress goes to none of; the engine's are the
        // switch's to add as they come (Switch::routes).
        subnets: {
            let mut all: Vec<(std::net::Ipv4Addr, u8)> = domains
                .iter()
                .filter_map(|d| d.link.as_ref())
                .flat_map(|l| l.addresses.iter().map(|a| (a.subnet, a.prefix)))
                .collect();
            all.sort_unstable();
            all.dedup();
            all
        },
        subnets6: {
            let mut all: Vec<(std::net::Ipv6Addr, u8)> = domains
                .iter()
                .filter_map(|d| d.link.as_ref())
                .flat_map(|l| l.addresses6.iter().map(|a| (a.subnet, a.prefix)))
                .collect();
            all.sort_unstable();
            all.dedup();
            all
        },
    }
}

/// Whether `d`'s flows past the microVM, out or in, or its names, cross init's namespace,
/// which the run's command shares, up the switch's link to eth0 ([`switch`]).
pub fn uplinked(d: &Domain) -> bool {
    d.link
        .as_ref()
        .is_some_and(|l| !l.egress.is_empty() || !l.ingress.is_empty() || l.dns || !l.named.is_empty())
}

/// The switch `domains` link to: `pairs` of them allowed to open connections to each
/// other, and those with egress grants up through init's namespace and eth0.
fn switch(
    domains: &[Domain],
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
) -> io::Result<crate::links::Switch> {
    let egress: Vec<(usize, Vec<crate::netplan::Egress>)> = domains
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            d.link
                .as_ref()
                .filter(|l| !l.egress.is_empty())
                .map(|l| (i, l.egress.clone()))
        })
        .collect();
    let ingress: Vec<(usize, Vec<crate::netplan::Ingress>)> = domains
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            d.link
                .as_ref()
                .filter(|l| !l.ingress.is_empty())
                .map(|l| (i, l.ingress.clone()))
        })
        .collect();
    // The switch forwards what comes in at the domain's own ports, past the uplink's
    // translation (D122).
    let ingress_at: Vec<(usize, Vec<crate::netplan::Egress>)> = ingress
        .iter()
        .map(|(i, ranges)| (*i, ranges.iter().map(|&(_, at)| at).collect()))
        .collect();
    let dns: Vec<usize> = domains
        .iter()
        .enumerate()
        .filter(|(_, d)| d.link.as_ref().is_some_and(|l| l.dns || !l.named.is_empty()))
        .map(|(i, _)| i)
        .collect();
    let uplink = match egress.is_empty() && ingress.is_empty() && dns.is_empty() {
        true => None,
        false => {
            let (addr, prefix, gateway) = crate::net::current()
                .ok_or_else(|| io::Error::other("the microVM has no address for its agents' egress"))?;
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            let mut subnets: Vec<(std::net::Ipv4Addr, u8)> = Vec::new();
            for a in domains
                .iter()
                .filter_map(|d| d.link.as_ref())
                .flat_map(|l| &l.addresses)
            {
                if !subnets.contains(&(a.subnet, a.prefix)) {
                    subnets.push((a.subnet, a.prefix));
                }
            }
            // Each to the domain's first address: every one of its addresses is its link's.
            let to_domain = ingress
                .iter()
                .filter_map(|(i, ranges)| {
                    let at = domains.get(*i)?.link.as_ref()?.addresses.first()?.addr;
                    Some((at, ranges.clone()))
                })
                .collect();
            let from_domain = egress
                .iter()
                .filter_map(|(i, ranges)| {
                    let from = domains
                        .get(*i)?
                        .link
                        .as_ref()?
                        .addresses
                        .iter()
                        .map(|a| a.addr)
                        .collect();
                    Some((from, ranges.clone()))
                })
                .collect();
            // By IPv6 too where eth0 has IPv6 and a domain with egress has IPv6 (D99).
            let from_domain6: Vec<(Vec<std::net::Ipv6Addr>, Vec<crate::netplan::Egress>)> = egress
                .iter()
                .filter_map(|(i, ranges)| {
                    let from: Vec<std::net::Ipv6Addr> = domains
                        .get(*i)?
                        .link
                        .as_ref()?
                        .addresses6
                        .iter()
                        .map(|a| a.addr)
                        .collect();
                    (!from.is_empty()).then(|| (from, ranges.clone()))
                })
                .collect();
            let ipv6 = match (crate::net::current6(), from_domain6.is_empty()) {
                (Some((eth0, _, _)), false) => {
                    let mut subnets6: Vec<(std::net::Ipv6Addr, u8)> = Vec::new();
                    for a in domains
                        .iter()
                        .filter_map(|d| d.link.as_ref())
                        .flat_map(|l| &l.addresses6)
                    {
                        if !subnets6.contains(&(a.subnet, a.prefix)) {
                            subnets6.push((a.subnet, a.prefix));
                        }
                    }
                    Some(crate::links::Uplink6 {
                        eth0,
                        subnets: subnets6,
                        egress: from_domain6,
                    })
                }
                _ => None,
            };
            Some(crate::links::Uplink {
                ingress: to_domain,
                egress: from_domain,
                resolver: !dns.is_empty(),
                subnets,
                eth0: (addr, std::net::Ipv4Addr::from(u32::from(addr) & mask), prefix),
                gateway,
                ipv6,
            })
        }
    };
    let resolver = uplink
        .as_ref()
        .and_then(|_| crate::net::current().map(|(_, _, g)| g));
    let ipv6 = domains
        .iter()
        .any(|d| d.link.as_ref().is_some_and(|l| !l.addresses6.is_empty()));
    // Every subnet the domains' networks hold, which the networks a microVM's engine
    // makes keep clear of (D181).
    let mut subnets: Vec<(std::net::Ipv4Addr, u8)> = Vec::new();
    for a in domains
        .iter()
        .filter_map(|d| d.link.as_ref())
        .flat_map(|l| &l.addresses)
    {
        if !subnets.contains(&(a.subnet, a.prefix)) {
            subnets.push((a.subnet, a.prefix));
        }
    }
    let switch = crate::links::Switch::new(
        pairs,
        &egress,
        &ingress_at,
        &dns,
        uplink,
        ipv6,
        domains.len(),
        &subnets,
    )?;
    // The agents' resolver, for those granted names.
    let mut askers: Vec<crate::agentdns::Asker> = Vec::new();
    for &i in &dns {
        let Some(l) = domains.get(i).and_then(|d| d.link.as_ref()) else {
            continue;
        };
        askers.push(crate::agentdns::Asker {
            link: crate::links::link_index(i)?,
            any: l.dns,
            // Its destinations' names, but the host itself's, which its hosts file holds
            // and no resolver is asked (D236).
            names: l
                .named
                .iter()
                .map(|(h, _)| h.clone())
                .filter(|h| h != shards_dns::HOST)
                .collect(),
            addrs: l.addresses.iter().map(|a| a.addr).collect(),
            local: Vec::new(),
        });
    }
    if let (false, Some(up)) = (askers.is_empty(), resolver) {
        let sockets = switch.resolver_sockets(std::net::SocketAddr::from((up, 53)))?;
        crate::agentdns::start(sockets, askers).map_err(io::Error::other)?;
    }
    Ok(switch)
}

/// Starts each of `domains`, hiding from each the directories of `all` but its own, under
/// the filter of `filters` it needs, which the host compiles (`domains-seccomp=`, and
/// `domains-seccomp-none=` for `--processes=none`); none starts without it. Those with a
/// link are linked once they exist, `pairs` of them allowed to open connections.
pub fn start(
    all: &[Vec<u8>],
    domains: &[Domain],
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
    filters: &[Option<Filter>; 2],
    setup: &[Vec<u8>],
) -> Result<Vec<Started>, String> {
    if domains.is_empty() {
        crate::staged::check(setup, |_| false)?;
        return Ok(Vec::new());
    }
    // What an orchestrator stages goes into a member this run starts, or nowhere (D231).
    crate::staged::check(setup, |m| {
        domains.iter().any(|d| d.id.checked_sub(FIRST_ID) == Some(m))
    })?;
    let shm = shm_of(setup)?;
    // The image's environment, which every member starts with (D59), as the host read it
    // from the image's config and the build and the daemon checked it.
    let image_env = shards_abi::run::members::image_envs(setup)
        .map_err(|e| format!("the image's environment: {e}"))?
        .into_iter()
        .map(|e| {
            String::from_utf8(e.to_vec()).map_err(|_| "the image's environment: a variable that is not UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The kernel's log holds what it says of the domains' processes (a fault, an OOM kill
    // naming each process of their cgroup): only for CAP_SYSLOG, which no run of an
    // Agentfile's image is given (D115), not for any process that calls syslog(2) or
    // reads /dev/kmsg, as with `dmesg_restrict` 0 (Documentation/admin-guide/sysctl/
    // kernel.rst), which a workload's `seccomp=unconfined` or `--device /dev/kmsg` would
    // leave it.
    crate::setup::write_sysctl("kernel.dmesg_restrict", "1")?;
    // The domains together take at most what the microVM can give as they start
    // (`MemAvailable`, Documentation/filesystems/proc.rst, and what it may still have
    // plugged, D148), less what the workload's own limit (`-m`) still promises it: never
    // what init and the workload hold or may. Their
    // scratch tmpfs counts, which the OOM killer's choice by resident memory does not
    // (mm/oom_kill.c, `oom_badness`), so the kernel does not choose: past `memory.high`
    // it throttles them and kills none (Documentation/admin-guide/cgroup-v2.rst), in the
    // charge itself as well as on the way back to user space (mm/memcontrol.c,
    // `try_charge_memcg`), and [`Memory`] ends the domain holding the most.
    let promised = admission::workload_headroom().map_err(|e| e.message("domains"))?;
    // What may still be plugged counted once: the kernel's own report of it taken off
    // `MemAvailable` (D149), whose kernel reports it.
    let [available] = crate::plug::unreported(|| available_memory().map(|a| [a]).map_err(io::Error::other))
        .map_err(|e| e.to_string())?;
    let available = admission::pressure_headroom(available, crate::plug::still_pluggable(), promised)
        .map_err(|e| e.message("domains"))?;
    // None of the microVM's swap (D159), which a run's limits give it for its own: a
    // domain's memory is what its Agentfile grants, and nothing grants one swap.
    let admission = admission::Parent::prepare(available).map_err(|e| e.message("domains"))?;
    let last_cap = crate::defaults::last_cap();
    landlock_abi()?;
    let lower =
        crate::changes::lower().ok_or("the image's layers were not kept, which a domain's root is")?;
    // The host's path roots (D235), beneath which paths are plugged into a domain as it runs.
    let roots = crate::hotplug::plug_roots(setup)?;
    let plug_roots: Vec<CString> = roots
        .iter()
        .map(|r| CString::new([&b"/"[..], r.to_bytes()].concat()))
        .collect::<Result<_, _>>()
        .map_err(|_| "a plug root with a NUL")?;
    let skel = skeleton(lower, &[], &roots).map_err(|e| format!("a domain's root: {e}"))?;
    // The Unix sockets granted (`CONNECT --port=unix:<name>`): a directory each, of the
    // tmpfs the run's writable layer lies in, which no path from the run's root reaches and
    // no mount of the run's shares; sticky, so that none removes another's.
    let shared = crate::changes::upper().map(|u| format!("{u}/../agents"));
    let mut made: Vec<(String, String)> = Vec::new();
    for d in domains {
        for (net, name, _) in d.link.as_ref().map(|l| l.unix.as_slice()).unwrap_or_default() {
            if made.iter().any(|(n, m)| n == net && m == name) {
                continue;
            }
            let base = shared
                .as_ref()
                .ok_or("the run's writable layer was not kept, where Unix sockets lie")?;
            let dir = format!("{base}/{net}/{name}");
            std::fs::create_dir_all(&dir).map_err(|e| format!("Unix socket {net}/{name}: {e}"))?;
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777))
                .map_err(|e| format!("Unix socket {net}/{name}: {e}"))?;
            made.push((net.clone(), name.clone()));
        }
    }
    // The in-VM server (§5, D60): an instance for each domain, from the read-only device
    // the daemon attaches to microVMs with agents, mounted beside the run's writable
    // layer, where no path from the run's root reaches, as the Unix sockets granted are.
    let rw = crate::changes::upper()
        .map(|u| format!("{u}/.."))
        .ok_or("the run's writable layer was not kept, where the in-VM server lies")?;
    let server = Server::mount(&rw)?;
    // The volumes the run gives its domains (D109): each mounted once, where no path from
    // the run's root reaches, as the server is; one the run's own root does not mount
    // filled from the image here, where it is empty, as the run fills its own; owned and
    // moded as `--chown` and `--chmod` say, each time the run starts.
    let lower_at = format!("/proc/self/fd/{}", lower.as_raw_fd());
    let built = crate::inroot::Root::open(std::path::Path::new(&lower_at))
        .map_err(|e| format!("the image's layers: {e}"))?;
    let passwd = built_file(&built, "/etc/passwd")?;
    let group = built_file(&built, "/etc/group")?;
    // The run's CA (D242), for each member whose link reaches a host the run's credential
    // grants name: the image's own bundle with the CA appended, made once.
    let trust = crate::trust::Run::of(setup).map_err(|e| e.to_string())?;
    let laid = match &trust {
        Some(t) if domains.iter().any(|d| t.reaches(d.link.as_ref())) => {
            Some(t.lay(&built).map_err(|e| e.to_string())?)
        }
        _ => None,
    };
    // Each share staged, and where it lies in the image.
    let mut staged: Vec<(Share, String, String)> = Vec::new();
    for (k, sh) in shares_of(setup).into_iter().enumerate() {
        if !domains.iter().any(|d| given_volume(d, &sh.dest)) {
            continue;
        }
        let fail = |why: String| format!("the volume at {}: {why}", sh.dest);
        // Where it lies in the image, resolved in it (securejoin): the image's symlinks on
        // the way followed in it, `..` never above it, so that a domain mounts it where the
        // run's root would, and keeps the image's own symlinks (`/var/run`).
        let real = built
            .resolve(sh.dest.as_bytes(), false)
            .map_err(|e| fail(e.to_string()))
            .and_then(|r| String::from_utf8(r).map_err(|_| fail("a name that is not UTF-8".into())))?;
        let declared = domains
            .iter()
            .any(|d| d.volumes.iter().any(|r| r.path == sh.dest));
        if !given_at(&sh.dest, &real, declared).map_err(|m| fail(m.to_string()))? {
            continue;
        }
        let at = format!("{rw}/volumes/{k}");
        std::fs::create_dir_all(&at).map_err(|e| fail(e.to_string()))?;
        let (tag, target) = (cstr_of(&sh.tag)?, cstr_of(&at)?);
        let flags = libc::MS_NOSUID | libc::MS_NODEV;
        // Writable to be told whose the host's files are, then read-only where it is.
        // SAFETY: mount(2) of NUL-terminated strings.
        if unsafe {
            libc::mount(
                tag.as_ptr(),
                target.as_ptr(),
                c"virtiofs".as_ptr(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(fail(io::Error::last_os_error().to_string()));
        }
        // A volume given one agent or harness alone, as a `VOLUME … FOR` scopes it, or the
        // run's own root does not mount: the files the host made there are its (D38), as a
        // run's are its user's, a bind standing in for its volume too. The domains start
        // once the run's own volumes are mounted, so this is said after the run's root
        // said its user's. One no `VOLUME` scopes, the run's own too, stays the run's
        // user's; one several domains share that the run's root does not mount, root's.
        let scoped = domains
            .iter()
            .any(|d| d.volumes.iter().any(|r| r.path == sh.dest));
        if (scoped || !sh.main)
            && let [only] = domains
                .iter()
                .filter(|d| given_volume(d, &sh.dest))
                .collect::<Vec<_>>()
                .as_slice()
        {
            crate::setup::name_owner_as(&at, only.id, only.id);
        }
        if sh.ro {
            // SAFETY: mount(2) of NUL-terminated strings: its flags changed, read-only.
            let remounted = unsafe {
                libc::mount(
                    c"".as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT | flags | libc::MS_RDONLY,
                    std::ptr::null(),
                )
            };
            if remounted != 0 {
                return Err(fail(io::Error::last_os_error().to_string()));
            }
        }
        let empty = || -> Result<bool, String> {
            Ok(std::fs::read_dir(&at)
                .map_err(|e| fail(e.to_string()))?
                .next()
                .is_none())
        };
        if !sh.main && sh.copy && !sh.ro && empty()? {
            // The image's directory there, found in the image (FollowSymlinkInScope): no
            // symlink of the image leads the fill out of it, to init's root or another's
            // volume.
            match built.open_at(real.as_bytes(), libc::O_PATH | libc::O_DIRECTORY) {
                Ok(dir) => {
                    // `/.` past the descriptor: the directory's own owner and mode.
                    let from = crate::inroot::path(&dir).join(".");
                    crate::setup::copy_tree(&from, std::path::Path::new(&at))
                        .map_err(|e| fail(format!("filling it from the image: {e}")))?;
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {}
                Err(e) => return Err(fail(format!("its directory in the image: {e}"))),
            }
        }
        if let Some(rule) = domains
            .iter()
            .flat_map(|d| d.volumes.iter())
            .find(|r| r.path == sh.dest)
            .filter(|_| sh.own)
        {
            if !rule.chown.is_empty() {
                let (uid, gid) = owner(&rule.chown, domains, passwd.as_deref(), group.as_deref())
                    .map_err(|e| fail(format!("--chown={}: {e}", rule.chown)))?;
                std::os::unix::fs::chown(&at, Some(uid), Some(gid))
                    .map_err(|e| fail(format!("--chown={}: {e}", rule.chown)))?;
            }
            if !rule.chmod.is_empty() {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = u32::from_str_radix(&rule.chmod, 8)
                    .ok()
                    .filter(|m| *m <= 0o7777)
                    .ok_or_else(|| fail(format!("--chmod={}: no octal mode", rule.chmod)))?;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| fail(format!("--chmod={}: {e}", rule.chmod)))?;
            }
        }
        staged.push((sh, at, real));
    }
    let no_processes = filters
        .get(1)
        .and_then(Option::as_ref)
        .ok_or("the run gave no seccomp filter for the in-VM server")?;
    // A socket pair for each two domains one may message, kept until both instances hold
    // theirs: messages pass only where the Agentfile grants them.
    let mut joined: Vec<((usize, usize), OwnedFd, OwnedFd)> = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        for &(j, _, _) in &d.channels {
            let key = (i.min(j), i.max(j));
            if joined.iter().any(|(k, _, _)| *k == key) {
                continue;
            }
            let mut fds = [0 as RawFd; 2];
            // SAFETY: socketpair(2) into a two-element array.
            if unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            } != 0
            {
                return Err(format!(
                    "{}: its server's channels: {}",
                    d.label,
                    io::Error::last_os_error()
                ));
            }
            // SAFETY: both descriptors were just made, and are owned here alone.
            let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
            joined.push((key, a, b));
        }
    }
    // Ports several domains are given go to the one listening (D122): the chooser, started
    // before any of them, and the address of each it chooses among.
    let contests = crate::chooser::contested(
        &domains
            .iter()
            .filter_map(|d| {
                let l = d.link.as_ref()?;
                Some((l.addresses.first()?.addr, l.ingress.clone()))
            })
            .filter(|(_, ranges)| !ranges.is_empty())
            .collect::<Vec<_>>(),
    );
    let candidates: Vec<std::net::Ipv4Addr> = contests
        .iter()
        .flat_map(|c| c.candidates.iter().map(|(a, _)| *a))
        .collect();
    if !contests.is_empty() && CHOOSER.get().is_none() {
        let chooser = crate::chooser::start(contests).map_err(|e| format!("the ports' chooser: {e}"))?;
        let _ = CHOOSER.set(chooser);
    }
    let notify_ops = crate::chooser::filter();
    let notify = libc::sock_fprog {
        len: u16::try_from(notify_ops.len()).map_err(|_| "the chooser's filter is too long")?,
        filter: notify_ops.as_ptr().cast_mut(),
    };
    // The members whose stdin their owner writes, each with its connection's token; every
    // one a member that starts here, else the run is refused, as a staging entry is.
    let inputs =
        shards_abi::run::managed::input_tokens(setup).map_err(|e| format!("its members' input: {e}"))?;
    if let Some((member, _)) = inputs
        .iter()
        .find(|(m, _)| !domains.iter().any(|d| d.id.checked_sub(FIRST_ID) == Some(*m)))
    {
        return Err(format!(
            "an input is asked for member {member}, which this image starts no program for"
        ));
    }
    // The members whose stdio is a terminal of their own (plan step 6, D244): each one that
    // starts here, else the run is refused, as an input is.
    let terminals =
        shards_abi::run::members::terminals(setup).map_err(|e| format!("its members' terminals: {e}"))?;
    if let Some(member) = terminals
        .iter()
        .find(|m| !domains.iter().any(|d| d.id.checked_sub(FIRST_ID) == Some(**m)))
    {
        return Err(format!(
            "a terminal is asked for member {member}, which this image starts no program for"
        ));
    }
    // The members with an MCP bridge and socket (D254): each one that starts here, else the
    // run is refused, as a terminal is.
    let mcp = crate::mcp::Asked::read(setup)?;
    if let Some(member) = mcp
        .iter()
        .flat_map(|a| &a.members)
        .find(|m| !domains.iter().any(|d| d.id.checked_sub(FIRST_ID) == Some(**m)))
    {
        return Err(format!(
            "an MCP bridge is asked for member {member}, which this image starts no program for"
        ));
    }
    // Their sockets, carried once every member has started, and their bridges' mounts, held
    // until each member's view has its own.
    let mut mcp_listeners = Vec::new();
    let mut bridges = Vec::new();
    let mut started = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let member =
            d.id.checked_sub(FIRST_ID)
                .ok_or("a domain's identity precedes its declared range")?;
        let group = format!("{CGROUPS}/{}", d.cgroup);
        let admitted = admission
            .prepare_child(&d.cgroup)
            .and_then(|p| p.commit(d.pids, d.memory))
            .map_err(|e| e.message(&d.label))?;
        let cgroup = admitted.group();
        let filter = filters
            .get(usize::from(d.no_processes))
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("{}: the run gave no seccomp filter for it", d.label))?;
        let seccomp_len =
            u16::try_from(filter.1.len()).map_err(|_| "the domains' seccomp filter is too long")?;
        let granted = d.link.as_ref().map(|l| l.unix.as_slice()).unwrap_or_default();
        let mut sockets = Vec::new();
        let mut copies = Vec::new();
        let mut receives = Vec::new();
        for (net, name, receiving) in granted {
            let target = cstr_of(&format!("run/networks/{net}/{name}"))?;
            let base = shared
                .as_ref()
                .ok_or("the run's writable layer was not kept, where Unix sockets lie")?;
            let quiet = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | libc::MOUNT_ATTR_NOEXEC;
            let attr = if *receiving {
                quiet
            } else {
                quiet | libc::MOUNT_ATTR_RDONLY
            };
            let copy = copy_of(&format!("{base}/{net}/{name}"), attr)
                .map_err(|e| format!("{}: Unix socket {net}/{name}: {e}", d.label))?;
            let at = cstr_of(&format!("/run/networks/{net}/{name}"))?;
            if *receiving {
                receives.push(at.clone());
            }
            sockets.push((copy.as_raw_fd(), at));
            copies.push((copy, target));
        }
        // Its server instance, listening before it starts, and its way there, read-only:
        // its socket, the instance's CA, its certificate and key.
        let peers: Vec<Peer<'_>> = d
            .channels
            .iter()
            .filter_map(|&(j, send, answer)| {
                let (_, a, b) = joined.iter().find(|(k, _, _)| *k == (i.min(j), i.max(j)))?;
                let end = if i < j { a } else { b };
                Some(Peer {
                    fd: end.as_raw_fd(),
                    label: domains.get(j)?.label.as_str(),
                    send,
                    answer,
                })
            })
            .collect();
        // Its scratch, which its instance holds too, to read the files it hands across.
        let scratch = scratch(lower, d.id).map_err(|e| format!("{}: its /tmp: {e}", d.label))?;
        // What its orchestrator staged for it (D231): its variables over its own and its
        // image's, its program found on the PATH it runs with, its working directory, and
        // its files in its home, where its HOME lies, before anything of it runs.
        let its_dir =
            std::str::from_utf8(&d.dir).map_err(|_| format!("{}: a directory that is not UTF-8", d.label))?;
        let own = harness_env(its_dir, &d.label, &image_env, &d.run_env)?
            .into_iter()
            .map(|e| CString::new(e).map_err(|_| format!("{}: a NUL in its environment", d.label)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut env = crate::staged::env(&own, member, setup).map_err(|e| format!("{}: {e}", d.label))?;
        // The run's CA, where its link reaches a host the run's credential grants name: its
        // bundle named by each variable its TLS stacks read roots from, and laid as it starts.
        let given = laid
            .as_ref()
            .filter(|_| trust.as_ref().is_some_and(|t| t.reaches(d.link.as_ref())));
        if let Some(l) = given {
            if scratch.is_none() {
                return Err(format!("{}: {}", d.label, crate::trust::Refused::NoScratch));
            }
            l.name_in(&mut env).map_err(|e| format!("{}: {e}", d.label))?;
        }
        let argv0 = executable(d, &env)?;
        // Its run-time arguments (D256): its command line points into them until it execs.
        let given_args = crate::staged::args(member, setup).map_err(|e| format!("{}: {e}", d.label))?;
        let workdir = crate::staged::workdir(member, setup)
            .map_err(|e| format!("{}: {e}", d.label))?
            .unwrap_or_else(|| d.workdir.clone());
        let home = env
            .iter()
            .rev()
            .find_map(|e| e.to_bytes().strip_prefix(b"HOME="))
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        // What it may write as it starts, each where it sees it: its scratch, at /tmp, and
        // each writable volume given it, at its mount point as declared and as it lies in
        // the image, with init's own mount of it.
        let mut writable: Vec<(&str, crate::staged::Mount<'_>)> = Vec::new();
        if let Some(s) = &scratch {
            writable.push(("/tmp", crate::staged::Mount::Held(s.as_fd())));
        }
        for (sh, at, real) in staged
            .iter()
            .filter(|(sh, _, _)| !sh.ro && given_volume(d, &sh.dest))
        {
            writable.extend([
                (real.as_str(), crate::staged::Mount::At(at.as_str())),
                (sh.dest.as_str(), crate::staged::Mount::At(at.as_str())),
            ]);
        }
        let points: Vec<&str> = writable.iter().map(|(p, _)| *p).collect();
        let place = home_in(&home, &points)
            .and_then(|(i, beneath)| {
                Some(crate::staged::Home {
                    mount: writable.get(i)?.1,
                    beneath,
                })
            })
            .ok_or_else(|| {
                format!("HOME, {home:?}, lies in neither its scratch nor a writable volume given it")
            });
        crate::staged::files(place, (d.id, d.id), member, setup).map_err(|e| format!("{}: {e}", d.label))?;
        let dir = server.start(
            i,
            d,
            no_processes,
            last_cap,
            &group,
            &peers,
            scratch.as_ref().map(AsRawFd::as_raw_fd),
        )?;
        // Its MCP socket and its bridge's mount point beside the server's (D254), where its run
        // asks for them: in its own directory, which no other member sees.
        let bridged = mcp.as_ref().is_some_and(|a| a.members.contains(&member));
        if bridged {
            let listener = crate::mcp::listen(std::path::Path::new(&dir), d.id)
                .map_err(|e| format!("{}: its MCP socket: {e}", d.label))?;
            mcp_listeners.push((listener, member));
        }
        let quiet = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | libc::MOUNT_ATTR_NOEXEC;
        let copy = copy_of(&dir, quiet | libc::MOUNT_ATTR_RDONLY)
            .map_err(|e| format!("{}: the in-VM server: {e}", d.label))?;
        sockets.push((copy.as_raw_fd(), cstr_of(SERVER_AT)?));
        copies.push((copy, cstr_of(SERVER_AT.trim_start_matches('/'))?));
        // Its bridge, this binary, mounted on the empty file beside its socket, after the
        // directory it lies in: runnable, where that directory is not.
        if bridged {
            let bridge =
                crate::mcp::bridge_mount().map_err(|e| format!("{}: its MCP bridge: {e}", d.label))?;
            sockets.push((bridge.as_raw_fd(), cstr_of(shards_abi::run::mcp::BRIDGE)?));
            bridges.push(bridge);
        }
        // Its volumes, each a copy of the run's mount of it, at its mount point.
        let mut volumes = Vec::new();
        let mut volume_rules = Vec::new();
        for (sh, at, real) in staged.iter().filter(|(sh, _, _)| given_volume(d, &sh.dest)) {
            let attr = libc::MOUNT_ATTR_NOSUID
                | libc::MOUNT_ATTR_NODEV
                | if sh.ro { libc::MOUNT_ATTR_RDONLY } else { 0 };
            let copy =
                copy_of(at, attr).map_err(|e| format!("{}: the volume at {}: {e}", d.label, sh.dest))?;
            volumes.push((copy.as_raw_fd(), cstr_of(real)?));
            volume_rules.push((cstr_of(real)?, sh.ro));
            copies.push((copy, cstr_of(real.trim_start_matches('/'))?));
        }
        let own_skel;
        let skel_of = if copies.is_empty() {
            &skel
        } else {
            let dirs: Vec<CString> = copies.iter().map(|(_, t)| t.clone()).collect();
            own_skel = skeleton(lower, &dirs, &roots).map_err(|e| format!("{}: its root: {e}", d.label))?;
            &own_skel
        };
        let root = image_root(skel_of, lower, !roots.is_empty())
            .map_err(|e| format!("{}: its root: {e}", d.label))?;
        let confinement = Confinement {
            id: d.id,
            last_cap,
            net_handled: match &d.link {
                Some(l) => {
                    (if l.connects { 0 } else { landlock::NET_CONNECT_TCP })
                        | (if l.accepts { 0 } else { landlock::NET_BIND_TCP })
                }
                None => landlock::NET_BIND_TCP | landlock::NET_CONNECT_TCP,
            },
            volumes: volume_rules,
            plug_roots: plug_roots.clone(),
            receives,
            seccomp: filter.clone(),
            seccomp_len,
            env: env.clone(),
            workdir,
        };
        let prepared = Prepared {
            shm: shm.clone(),
            root: root.as_raw_fd(),
            sockets,
            volumes,
            scratch: scratch.as_ref().map(AsRawFd::as_raw_fd),
            hide: all
                .iter()
                .filter(|x| **x != d.dir)
                .flat_map(|x| [x.clone(), [x.as_slice(), b".d"].concat()])
                .filter_map(|x| CString::new(x).ok())
                .collect(),
            hostname: CString::new(d.cgroup.as_str())
                .map_err(|_| format!("{}: a NUL in its name", d.label))?,
            argv: command_line(&argv0, d, &given_args)
                .into_iter()
                .map(CStr::as_ptr)
                .chain([std::ptr::null()])
                .collect(),
            envp: env.iter().map(|a| a.as_ptr()).chain([std::ptr::null()]).collect(),
            hosts: d.link.as_ref().map(|l| l.hosts.clone()),
            resolv: d.resolv.clone(),
            trust: given.map(|l| (l.path.clone(), l.bundle.clone())),
            go: None,
            chooser: None,
            stdin: None,
            terminal: false,
        };
        let mut prepared = prepared;
        // A candidate for a port several are given: the channel its listener comes on.
        let address = d.link.as_ref().and_then(|l| l.addresses.first()).map(|a| a.addr);
        let chooser_channel = match address.filter(|a| candidates.contains(a)) {
            Some(addr) => {
                let (ours, theirs) = crate::hostbind::seqpacket()
                    .map_err(|e| format!("{}: its chooser's channel: {e}", d.label))?;
                prepared.chooser = Some((theirs.as_raw_fd(), &raw const notify));
                Some((addr, ours, theirs))
            }
            None => None,
        };
        let go = match &d.link {
            Some(_) => {
                let mut fds = [0 as RawFd; 2];
                // SAFETY: pipe2(2) into a two-element array.
                if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                    return Err(format!(
                        "{}: its link's pipe: {}",
                        d.label,
                        io::Error::last_os_error()
                    ));
                }
                // SAFETY: both descriptors were just made, and are owned here alone.
                let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
                prepared.go = Some(r.as_raw_fd());
                Some((r, w))
            }
            None => None,
        };
        let (stderr, stderr_write) =
            crate::capture::Pipe::make().map_err(|e| format!("{}: its stderr pipe: {e}", d.label))?;
        // Its stdout: a pipe init reads, or a terminal of its own (D244), whose master init
        // reads as its stdout and whose other end, owned by its uid, is all its stdio, so
        // that its stderr channel ends at once. The kernel makes a terminal 0 by 0; its owner
        // sizes it (`builtin::RESIZE`).
        let terminal = terminals
            .contains(&member)
            .then(|| crate::enter::terminal(shards_abi::run::Size::default(), d.id))
            .transpose()
            .map_err(|e| format!("{}: its terminal: {e}", d.label))?;
        let (stdout, stdout_write, master) = match terminal {
            Some((master, other)) => {
                let kept = master
                    .try_clone()
                    .map_err(|e| format!("{}: its terminal: {e}", d.label))?;
                let read = crate::capture::Pipe::master(master)
                    .map_err(|e| format!("{}: its terminal: {e}", d.label))?;
                (read, other, Some(kept))
            }
            None => {
                let (read, write) =
                    crate::capture::Pipe::make().map_err(|e| format!("{}: its stdout pipe: {e}", d.label))?;
                (read, write, None)
            }
        };
        let (input, stdin) = match (inputs.iter().find(|(m, _)| *m == member), &master) {
            (Some((_, token)), Some(master)) => {
                let typed = master
                    .try_clone()
                    .and_then(|m| crate::input::Input::terminal(member, token, m))
                    .map_err(|e| format!("{}: its stdin: {e}", d.label))?;
                (Some(typed), None)
            }
            (Some((_, token)), None) => {
                let (input, stdin) = crate::input::Input::open(member, token)
                    .map_err(|e| format!("{}: its stdin pipe: {e}", d.label))?;
                (Some(input), Some(stdin))
            }
            (None, _) => (None, None),
        };
        prepared.stdin = stdin.as_ref().map(AsRawFd::as_raw_fd);
        prepared.terminal = master.is_some();
        // A terminal's other end is its stderr too, and where its setup's failure is said;
        // its stderr pipe's write end is no one's, so the channel ends at once.
        let out = if master.is_some() {
            stdout_write.as_raw_fd()
        } else {
            stderr_write.as_raw_fd()
        };
        let mut launch = crate::managed::Launch::new(last_cap, cgroup)
            .map_err(|e| format!("{}: preparing its owner: {e}", d.label))?;
        let mut args = CloneArgs {
            pidfd: (&raw mut launch.pidfd) as u64,
            flags: (libc::CLONE_PIDFD
                | libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWUTS
                | libc::CLONE_NEWCGROUP) as u64
                | CLONE_INTO_CGROUP,
            exit_signal: libc::SIGCHLD as u64,
            cgroup: cgroup.as_raw_fd() as u64,
            ..CloneArgs::default()
        };
        // SAFETY: clone3(2) with a clone_args of the size given. The child calls only the
        // kernel, on what `prepared` holds, and execs or exits.
        let pid = unsafe { libc::syscall(libc::SYS_clone3, &raw mut args, std::mem::size_of::<CloneArgs>()) };
        if pid < 0 {
            return Err(format!("starting {}: {}", d.label, io::Error::last_os_error()));
        }
        if pid == 0 {
            let setup = launch.child();
            child(d, &prepared, &confinement, stdout_write.as_raw_fd(), out, setup);
        }
        let pid = libc::pid_t::try_from(pid)
            .map_err(|_| format!("{}: the kernel returned a PID out of range", d.label))?;
        let mut lifecycle = launch
            .parent(pid, admitted.into_file())
            .map_err(|e| format!("{}: retaining its owner: {e}", d.label))?;
        // Its stdin's read end is the member's alone: init holding it would keep the pipe
        // read after the member is gone, and so hide its end.
        drop(stdin);
        drop(stdout_write);
        drop(stderr_write);
        drop(root);
        drop(copies);
        if let (Some(link), Some((r, w))) = (&d.link, go) {
            drop(r);
            let switch = match SWITCH.get() {
                Some(s) => Ok(s),
                None => switch(domains, pairs).map(|s| SWITCH.get_or_init(|| s)),
            };
            let gate = gate_of(domains, pairs, i, link);
            let linked = switch.and_then(|s| s.attach(i, pid as libc::pid_t, link, &gate));
            if let Err(e) = linked {
                // SAFETY: kill(2) of the child just made, which waits for its link.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                return Err(format!("{}: its link: {e}", d.label));
            }
            // SAFETY: write(2) of one byte to the pipe it waits on.
            if unsafe { libc::write(w.as_raw_fd(), b"g".as_ptr().cast(), 1) } != 1 {
                return Err(format!(
                    "{}: its link's pipe: {}",
                    d.label,
                    io::Error::last_os_error()
                ));
            }
        }
        // Its listener, sent as it loads the chooser's filter, and its network namespace, to
        // the chooser. One that ends before it sends its listener has said why on its output.
        if let Some((addr, ours, theirs)) = chooser_channel {
            drop(theirs);
            let mut buf = [0u8; 4];
            if let Ok((_, Some(listener))) = crate::hostbind::recv_fd(ours.as_raw_fd(), &mut buf, 0) {
                let netns = crate::links::open_ns(&format!("/proc/{pid}/ns/net"))
                    .map_err(|e| format!("{}: its network namespace: {e}", d.label))?;
                if !CHOOSER.get().is_some_and(|c| c.give(addr, &listener, &netns)) {
                    // SAFETY: kill(2) of the child just made, whose listens no one answers.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    return Err(format!("{}: the ports' chooser has gone", d.label));
                }
            }
        }
        lifecycle
            .launched()
            .map_err(|e| format!("{}: its launch: {e}", d.label))?;
        started.push(Started {
            member,
            output_control: crate::capture::OutputControl::Active,
            label: d.label.clone(),
            cgroup: group.clone(),
            lifecycle,
            stdout,
            stderr,
            input,
            end_told: false,
            confinement,
            terminal: master,
        });
    }
    drop(joined);
    drop(bridges);
    // Each member's MCP connections carried to its host from here on, every member started
    // (D254).
    if let Some(asked) = &mcp {
        for (listener, member) in mcp_listeners {
            crate::mcp::serve(listener, member, asked.token)
                .map_err(|e| format!("member {member}'s MCP socket: {e}"))?;
        }
    }
    Ok(started)
}

/// One of an instance's channels to another domain's: its end, the other's label, and
/// whether it may send them requests and answers theirs.
struct Peer<'a> {
    fd: RawFd,
    label: &'a str,
    send: bool,
    answer: bool,
}

/// Where a domain finds its server instance: its socket, the instance's CA, and its own
/// certificate and key.
const SERVER_AT: &str = "/run/shards";
/// The device the in-VM server lies on, which the daemon attaches after the root
/// filesystem's (vm_run.rs): its number, as sysfs gives it. The run's /dev holds its own
/// devices alone, as a container's does, so init makes a node of its own for it.
const SERVER_DEVICE: &str = "/sys/block/pmem1/dev";

/// The in-VM server's binary, mounted, and where its instances' directories go.
struct Server {
    binary: OwnedFd,
    base: String,
}

impl Server {
    /// Mounts the server's device read-only (EROFS, DAX: its pages are the host's, not the
    /// guest's memory), once, under `rw`.
    fn mount(rw: &str) -> Result<Server, String> {
        let number = std::fs::read_to_string(SERVER_DEVICE)
            .map_err(|e| format!("the in-VM server's device ({SERVER_DEVICE}): {e}"))?;
        let (major, minor) = number
            .trim()
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
            .ok_or_else(|| format!("the in-VM server's device: {SERVER_DEVICE} says {number:?}"))?;
        let node = cstr_of(&format!("{rw}/server-device"))?;
        // SAFETY: mknod(2) of a NUL-terminated path; an old one from a restored template
        // is the same device.
        if unsafe { libc::mknod(node.as_ptr(), libc::S_IFBLK | 0o600, libc::makedev(major, minor)) } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return Err(format!(
                "the in-VM server's device: {}",
                io::Error::last_os_error()
            ));
        }
        let at = format!("{rw}/server-binary");
        std::fs::create_dir_all(&at).map_err(|e| format!("{at}: {e}"))?;
        let target = cstr_of(&at)?;
        // SAFETY: mount(2) of NUL-terminated strings.
        if unsafe {
            libc::mount(
                node.as_ptr(),
                target.as_ptr(),
                c"erofs".as_ptr(),
                libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                c"dax=always".as_ptr().cast(),
            )
        } != 0
        {
            return Err(format!(
                "mounting the in-VM server's device: {}",
                io::Error::last_os_error()
            ));
        }
        let path = cstr_of(&format!("{at}/shards-server"))?;
        // SAFETY: open(2) of a NUL-terminated path, its descriptor owned below.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "the in-VM server's binary: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Server {
            // SAFETY: the descriptor open just made, owned here alone.
            binary: unsafe { OwnedFd::from_raw_fd(fd) },
            base: format!("{rw}/server"),
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an instance's whole start: its domain, its filter, its channels and its domain's scratch"
    )]
    /// Starts the `n`-th domain's instance, least-privileged (its own uid and gid, the
    /// domain's group to give its files to, no capability, `no_new_privs`, its own
    /// network, IPC and UTS namespaces), confined by `filter` once it listens; returns
    /// the directory it listens in, once it does.
    fn start(
        &self,
        n: usize,
        d: &Domain,
        filter: &Filter,
        last_cap: u32,
        group: &str,
        peers: &[Peer<'_>],
        scratch: Option<RawFd>,
    ) -> Result<String, String> {
        let fail = |e: String| format!("{}: its server: {e}", d.label);
        let id = u32::try_from(n)
            .ok()
            .and_then(|n| shards_abi::SERVER_FIRST_ID.checked_add(n))
            .ok_or_else(|| fail("no uid left".into()))?;
        let dir = format!("{}/{}", self.base, d.cgroup);
        std::fs::create_dir_all(&dir).map_err(|e| fail(format!("{dir}: {e}")))?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))
            .map_err(|e| fail(e.to_string()))?;
        std::os::unix::fs::chown(&dir, Some(id), Some(d.id)).map_err(|e| fail(e.to_string()))?;
        // Its directory, which no path of its own reaches, as its descriptor 6.
        let dir_path = cstr_of(&dir)?;
        // SAFETY: open(2) of a NUL-terminated path, its descriptor owned below.
        let dir_fd = unsafe {
            libc::open(
                dir_path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if dir_fd < 0 {
            return Err(fail(format!("{dir}: {}", io::Error::last_os_error())));
        }
        // SAFETY: the descriptor open just made, owned here alone.
        let dir_fd = unsafe { OwnedFd::from_raw_fd(dir_fd) };
        let argv_owned = [
            cstr_of("shards-server")?,
            cstr_of(&d.label)?,
            cstr_of("/proc/self/fd/6")?,
            cstr_of(&d.id.to_string())?,
            cstr_of("3")?,
            cstr_of("4")?,
            // Its channels, from descriptor 7 on: one a line, its descriptor, the other's
            // label, and whether it sends and answers.
            cstr_of(
                &peers
                    .iter()
                    .enumerate()
                    .map(|(k, p)| format!("{}\t{}\t{}\t{}\n", 7 + k, p.label, p.send, p.answer))
                    .collect::<String>(),
            )?,
            // The MCP servers offered its domain.
            cstr_of(&d.mcp)?,
            // Its domain's scratch, past its channels, where the image has one (D139).
            cstr_of(&scratch.map_or(String::new(), |_| (7 + peers.len()).to_string()))?,
        ];
        let argv: Vec<*const libc::c_char> = argv_owned
            .iter()
            .map(|a| a.as_ptr())
            .chain([std::ptr::null()])
            .collect();
        let envp = [std::ptr::null::<libc::c_char>()];
        let pipe = || -> Result<(OwnedFd, OwnedFd), String> {
            let mut fds = [0 as RawFd; 2];
            // SAFETY: pipe2(2) into a two-element array.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(fail(io::Error::last_os_error().to_string()));
            }
            // SAFETY: both descriptors were just made, and are owned here alone.
            Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
        };
        let (ready_r, ready_w) = pipe()?;
        let (filter_r, filter_w) = pipe()?;
        // What it says before it listens, if it ends: its stderr until then.
        let (said_r, said_w) = pipe()?;
        let groups = [d.id];
        // Its domain's cgroup, so that what an agent makes its instance hold counts against
        // the agent's own memory, and ends with it.
        let procs = cstr_of(&format!("{group}/cgroup.procs"))?;
        let ends: Vec<RawFd> = peers.iter().map(|p| p.fd).collect();
        let first_free = u32::try_from(7 + ends.len() + usize::from(scratch.is_some()))
            .map_err(|_| fail("too many channels".into()))?;
        // SAFETY: fork(2) from init with no other thread running; the child calls only the
        // kernel, on what was made above, and execs or exits.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(fail(io::Error::last_os_error().to_string()));
        }
        if pid == 0 {
            // SAFETY: system calls on descriptors and buffers made before the fork.
            unsafe {
                // Its channels above the descriptors they go to (7 on), before any is
                // placed, so that placing one overwrites none of them; then where they go.
                let mut high = Vec::with_capacity(ends.len());
                for &fd in &ends {
                    let h = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, first_free as libc::c_int);
                    if h < 0 {
                        libc::_exit(126);
                    }
                    high.push(h);
                }
                // Its domain's scratch, likewise above where it goes (D139).
                let scratch_high = match scratch {
                    Some(at) => {
                        let h = libc::fcntl(at, libc::F_DUPFD_CLOEXEC, first_free as libc::c_int);
                        if h < 0 {
                            libc::_exit(126);
                        }
                        Some(h)
                    }
                    None => None,
                };
                let up = |fd: RawFd| libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
                let (r, f, b, e, w) = (
                    up(ready_w.as_raw_fd()),
                    up(filter_r.as_raw_fd()),
                    up(self.binary.as_raw_fd()),
                    up(said_w.as_raw_fd()),
                    up(dir_fd.as_raw_fd()),
                );
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                if r < 0
                    || f < 0
                    || b < 0
                    || e < 0
                    || w < 0
                    || null < 0
                    || libc::dup2(null, 0) < 0
                    || libc::dup2(null, 1) < 0
                    || libc::dup2(e, 2) < 0
                    || libc::dup2(r, 3) < 0
                    || libc::dup2(f, 4) < 0
                    || libc::dup2(b, 5) < 0
                    || libc::fcntl(5, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                    || libc::dup2(w, 6) < 0
                {
                    libc::_exit(126);
                }
                for (k, &h) in high.iter().enumerate() {
                    if libc::dup2(h, 7 + k as libc::c_int) < 0 {
                        libc::_exit(126);
                    }
                }
                if let Some(h) = scratch_high
                    && libc::dup2(h, 7 + high.len() as libc::c_int) < 0
                {
                    libc::_exit(126);
                }
                let join = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if join < 0
                    || libc::write(join, c"0".as_ptr().cast(), 1) != 1
                    || libc::close(join) != 0
                    || libc::syscall(libc::SYS_close_range, first_free, u32::MAX, 0u32) != 0
                    || libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS) != 0
                    || !crate::defaults::bound(last_cap, |_| false)
                    || libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setresgid(id, id, id) != 0
                    || libc::setresuid(id, id, id) != 0
                    || !crate::defaults::set(last_cap, |_| false)
                    || libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                {
                    libc::_exit(126);
                }
                libc::syscall(
                    libc::SYS_execveat,
                    5,
                    c"".as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                    libc::AT_EMPTY_PATH,
                );
                libc::_exit(127)
            }
        }
        drop(ready_w);
        drop(filter_r);
        drop(said_w);
        // Its filter, as init's setup carries one: flags, then the program.
        let mut bytes = filter.0.to_le_bytes().to_vec();
        for i in &filter.1 {
            bytes.extend_from_slice(&i.code.to_ne_bytes());
            bytes.extend_from_slice(&[i.jt, i.jf]);
            bytes.extend_from_slice(&i.k.to_ne_bytes());
        }
        use std::io::{Read as _, Write as _};
        std::fs::File::from(filter_w)
            .write_all(&bytes)
            .map_err(|e| fail(format!("giving it its filter: {e}")))?;
        let mut said = [0u8; 1];
        match std::fs::File::from(ready_r).read(&mut said) {
            Ok(1) => Ok(dir),
            _ => {
                // Why: its status (126 a step before its exec, 127 the exec), and what it
                // wrote.
                let mut status = 0;
                // SAFETY: waitpid(2) of the child just forked, which has ended or will.
                unsafe { libc::waitpid(pid, &raw mut status, 0) };
                let mut text = String::new();
                let _ = std::fs::File::from(said_r).take(4096).read_to_string(&mut text);
                Err(fail(format!(
                    "it ended before it listened (status {}): {}",
                    libc::WEXITSTATUS(status),
                    text.trim()
                )))
            }
        }
    }
}

/// Writes `shards-init: <what>: errno <n>` to `out` and exits 125, without allocating.
fn fail(out: RawFd, setup: RawFd, step: crate::managed::Step, what: &CStr) -> ! {
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    crate::managed::report_failure(setup, step, errno);
    let mut digits = [0u8; 12];
    let mut at = digits.len();
    let mut v = errno.unsigned_abs();
    loop {
        at -= 1;
        if let Some(d) = digits.get_mut(at) {
            *d = b'0' + (v % 10) as u8;
        }
        v /= 10;
        if v == 0 || at == 0 {
            break;
        }
    }
    let parts: [&[u8]; 5] = [
        b"shards-init: ",
        what.to_bytes(),
        b": errno ",
        digits.get(at..).unwrap_or_default(),
        b"\n",
    ];
    for p in parts {
        // SAFETY: write(2) of a buffer it borrows.
        unsafe { libc::write(out, p.as_ptr().cast(), p.len()) };
    }
    // SAFETY: _exit(2), past which nothing runs.
    unsafe { libc::_exit(125) }
}

/// A domain's first process, before its program.
fn child(d: &Domain, p: &Prepared, c: &Confinement, stdout: RawFd, out: RawFd, setup: RawFd) -> ! {
    let fail = |out, what| -> ! { fail(out, setup, crate::managed::Step::Setup, what) };
    let null = std::ptr::null::<libc::c_char>();
    let nil = std::ptr::null::<libc::c_void>();
    let tmpfs = c"tmpfs".as_ptr();
    // SAFETY: each call below is a system call on NUL-terminated strings and buffers made
    // before the clone, in a process of one thread.
    unsafe {
        // Nothing of these mounts reaches init's namespace.
        if libc::mount(null, c"/".as_ptr(), null, libc::MS_REC | libc::MS_PRIVATE, nil) != 0 {
            fail(out, c"making its mounts private");
        }
        // The image as built, over the system's /proc, then its root; the system's mounts
        // under it detached (pivot_root(2), NOTES).
        if libc::syscall(
            libc::SYS_move_mount,
            p.root,
            c"".as_ptr(),
            libc::AT_FDCWD,
            c"/proc".as_ptr(),
            mount_api::MOVE_MOUNT_F_EMPTY_PATH,
        ) != 0
            || libc::chdir(c"/proc".as_ptr()) != 0
            || libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0
            || libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0
            || libc::chdir(c"/".as_ptr()) != 0
        {
            fail(out, c"entering the image as built");
        }
        // The microVM's system, read-only, nosuid and nodev (struct mount_attr).
        let attr: [u64; 4] = [SYSTEM, 0, 0, 0];
        if libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            c"/".as_ptr(),
            libc::AT_RECURSIVE,
            attr.as_ptr(),
            std::mem::size_of_val(&attr),
        ) != 0
        {
            fail(out, c"making the system read-only");
        }
        let shut = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        let empty = c"size=4k,nr_inodes=1,mode=000".as_ptr().cast();
        // Every other domain, and /sys, hidden.
        for h in &p.hide {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(h.as_ptr(), &raw mut st) == 0
                && libc::mount(tmpfs, h.as_ptr(), tmpfs, shut, empty) != 0
            {
                fail(out, c"hiding another domain");
            }
        }
        if libc::mount(tmpfs, c"/sys".as_ptr(), tmpfs, shut, empty) != 0 {
            fail(out, c"hiding /sys");
        }
        // Its PID namespace's /proc.
        let quiet = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        if libc::mount(c"proc".as_ptr(), c"/proc".as_ptr(), c"proc".as_ptr(), quiet, nil) != 0 {
            fail(out, c"mounting its /proc");
        }
        // Its /dev.
        let dev = c"mode=755,size=64k".as_ptr().cast();
        if libc::mount(
            tmpfs,
            c"/dev".as_ptr(),
            tmpfs,
            libc::MS_NOSUID | libc::MS_NOEXEC,
            dev,
        ) != 0
        {
            fail(out, c"mounting its /dev");
        }
        for (path, major, minor) in DEV {
            if libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) != 0
                || libc::chmod(path.as_ptr(), 0o666) != 0
            {
                fail(out, c"making its /dev");
            }
        }
        for (target, path) in LINKS {
            if libc::symlink(target.as_ptr(), path.as_ptr()) != 0 {
                fail(out, c"linking its /dev");
            }
        }
        if libc::mkdir(c"/dev/shm".as_ptr(), 0o755) != 0 || libc::mkdir(c"/dev/mqueue".as_ptr(), 0o755) != 0 {
            fail(out, c"making its /dev/shm and /dev/mqueue");
        }
        let ro_dev = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC;
        if libc::mount(null, c"/dev".as_ptr(), null, ro_dev, nil) != 0 {
            fail(out, c"making its /dev read-only");
        }
        // Its own scratch: /tmp, init's mount of it, which its server instance holds too
        // (D139); and /dev/shm.
        if let Some(fd) = p.scratch
            && libc::syscall(
                libc::SYS_move_mount,
                fd,
                c"".as_ptr(),
                libc::AT_FDCWD,
                c"/tmp".as_ptr(),
                mount_api::MOVE_MOUNT_F_EMPTY_PATH,
            ) != 0
        {
            fail(out, c"making its scratch");
        }
        for (path, data) in [(c"/dev/shm", p.shm.as_ptr())] {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(path.as_ptr(), &raw mut st) != 0 {
                continue;
            }
            if libc::mount(
                tmpfs,
                path.as_ptr(),
                tmpfs,
                libc::MS_NOSUID | libc::MS_NODEV,
                data.cast(),
            ) != 0
                || libc::chown(path.as_ptr(), d.id, d.id) != 0
            {
                fail(out, c"making its scratch");
            }
        }
        // Its POSIX message queues, of its own IPC namespace, as a Docker container's
        // (moby daemon/pkg/oci/defaults.go): mq_open makes them in the same filesystem
        // (ipc/mqueue.c, the namespace's `mq_mnt`), which Landlock lets it write beneath.
        if libc::mount(
            c"mqueue".as_ptr(),
            c"/dev/mqueue".as_ptr(),
            c"mqueue".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            nil,
        ) != 0
        {
            fail(out, c"mounting its /dev/mqueue");
        }
        // Its names: its peers', over the system's /etc/hosts, and where it reaches past
        // the microVM its resolver, over /etc/resolv.conf; each read-only, made in its
        // scratch and left there by no name.
        let ro = libc::MS_REMOUNT
            | libc::MS_BIND
            | libc::MS_RDONLY
            | libc::MS_NOSUID
            | libc::MS_NODEV
            | libc::MS_NOEXEC;
        for (content, target, made, writing, mounting) in [
            (
                &p.hosts,
                c"/etc/hosts",
                c"/tmp/.hosts",
                c"writing its /etc/hosts",
                c"mounting its /etc/hosts",
            ),
            (
                &p.resolv,
                c"/etc/resolv.conf",
                c"/tmp/.resolv",
                c"writing its /etc/resolv.conf",
                c"mounting its /etc/resolv.conf",
            ),
        ] {
            let mut st: libc::stat = std::mem::zeroed();
            let Some(content) = content else { continue };
            if libc::stat(target.as_ptr(), &raw mut st) != 0 || libc::stat(c"/tmp".as_ptr(), &raw mut st) != 0
            {
                continue;
            }
            let fd = libc::open(
                made.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o444,
            );
            if fd < 0 || libc::write(fd, content.as_ptr().cast(), content.len()) != content.len() as isize {
                fail(out, writing);
            }
            libc::close(fd);
            if libc::mount(made.as_ptr(), target.as_ptr(), null, libc::MS_BIND, nil) != 0
                || libc::mount(null, target.as_ptr(), null, ro, nil) != 0
                || libc::unlink(made.as_ptr()) != 0
            {
                fail(out, mounting);
            }
        }
        // Its trust bundle with the run's CA (D242), over its image's, read-only, made as its
        // names are; never passed over, as a member given it does not start without it (its
        // bundle and its scratch were found before the clone).
        if let Some((target, content)) = &p.trust {
            let made = c"/tmp/.trust";
            let fd = libc::open(
                made.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o444,
            );
            if fd < 0 || libc::write(fd, content.as_ptr().cast(), content.len()) != content.len() as isize {
                fail(out, c"writing its trust bundle");
            }
            libc::close(fd);
            if libc::mount(made.as_ptr(), target.as_ptr(), null, libc::MS_BIND, nil) != 0
                || libc::mount(null, target.as_ptr(), null, ro, nil) != 0
                || libc::unlink(made.as_ptr()) != 0
            {
                fail(out, c"mounting its trust bundle");
            }
        }
        // Its Unix sockets' directories, each its own mount, read-only where it only
        // connects: connecting needs no write to the directory, and making a socket does.
        for (copy, at) in &p.sockets {
            if libc::syscall(
                libc::SYS_move_mount,
                *copy,
                c"".as_ptr(),
                libc::AT_FDCWD,
                at.as_ptr(),
                mount_api::MOVE_MOUNT_F_EMPTY_PATH,
            ) != 0
            {
                fail(out, c"mounting its Unix sockets");
            }
        }
        // Its volumes, at their mount points.
        for (copy, at) in &p.volumes {
            if libc::syscall(
                libc::SYS_move_mount,
                *copy,
                c"".as_ptr(),
                libc::AT_FDCWD,
                at.as_ptr(),
                mount_api::MOVE_MOUNT_F_EMPTY_PATH,
            ) != 0
            {
                fail(out, c"mounting its volumes");
            }
        }
        // Its own name, and loopback.
        if libc::sethostname(p.hostname.as_ptr(), p.hostname.as_bytes().len()) != 0 {
            fail(out, c"naming its host");
        }
        if !crate::run::loopback_up_raw() {
            fail(out, c"bringing its loopback up");
        }
        // Its link, which init makes once it exists.
        if let Some(go) = p.go {
            let mut byte = 0u8;
            if libc::read(go, (&raw mut byte).cast(), 1) != 1 {
                fail(out, c"waiting for its link");
            }
        }
        // The chooser's filter (D122), whose listener goes to init on its channel, which
        // closes with init's other descriptors below; nothing this process does before its
        // command runs binds or listens.
        if let Some((channel, notify)) = p.chooser {
            let listener = libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                crate::chooser::NEW_LISTENER,
                notify,
            );
            let Ok(listener) = RawFd::try_from(listener) else {
                fail(out, c"loading the chooser's filter");
            };
            if listener < 0 {
                fail(out, c"loading the chooser's filter");
            }
            let sent = crate::hostbind::send_fd(channel, b"L", listener, 0);
            libc::close(listener);
            if !sent {
                fail(out, c"giving init the chooser's listener");
            }
        }
        // A terminal of its own (D244) is its session's controlling terminal, as an exec's
        // is made (run.rs `child`, TIOCSCTTY after setsid(2)), and its stdin.
        if p.terminal && (libc::setsid() < 0 || libc::ioctl(stdout, libc::TIOCSCTTY, 0) != 0) {
            fail(out, c"making its terminal its session's");
        }
        // Its stdio, and no other descriptor of init's.
        let input = match (p.terminal, p.stdin) {
            (true, _) => stdout,
            (false, Some(fd)) => fd,
            (false, None) => libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC),
        };
        let status = libc::fcntl(setup, libc::F_DUPFD_CLOEXEC, libc::STDERR_FILENO + 2);
        if input < 0
            || status < 0
            || libc::dup2(input, 0) < 0
            || libc::dup2(stdout, 1) < 0
            || libc::dup2(out, 2) < 0
        {
            fail(out, c"giving it its stdio");
        }
        if libc::dup3(status, libc::STDERR_FILENO + 1, libc::O_CLOEXEC) < 0 {
            fail(2, c"retaining its launch status");
        }
        if libc::syscall(
            libc::SYS_close_range,
            (libc::STDERR_FILENO + 2) as u32,
            u32::MAX,
            0u32,
        ) != 0
        {
            crate::domains::fail(
                2,
                libc::STDERR_FILENO + 1,
                crate::managed::Step::Setup,
                c"closing init's descriptors",
            );
        }
        exec_child(p, c)
    }
}

fn exec_child(p: &Prepared, c: &Confinement) -> ! {
    let setup = libc::STDERR_FILENO + 1;
    let out = libc::STDERR_FILENO;
    let fail = |out, what| -> ! { fail(out, setup, crate::managed::Step::Setup, what) };
    // SAFETY: the actor's remaining confinement uses prepared buffers and system calls.
    unsafe {
        if libc::chdir(c.workdir.as_ptr()) != 0 {
            fail(out, c"entering its working directory");
        }
        if let Err(step) = confine(c) {
            fail(out, step.what());
        }
        libc::execve(
            p.argv.first().copied().unwrap_or(std::ptr::null()),
            p.argv.as_ptr(),
            p.envp.as_ptr(),
        );
        crate::domains::fail(out, setup, crate::managed::Step::Exec, c"running its command")
    }
}

/// What confines a member's processes past the namespaces and cgroup they share (D59): its
/// IDs, no capability, `no_new_privs`, its umask, its Landlock rules and its seccomp
/// filter; and its environment and working directory as it started. Kept in its
/// [`Started`] past its start, so that a command run in it ([`crate::enter`]) is confined by the
/// same values, through the same [`confine`], as its own first process was.
pub struct Confinement {
    /// Its uid and gid, the n-th domain's `FIRST_ID + n`.
    pub id: u32,
    last_cap: u32,
    /// The TCP rights Landlock refuses it: connecting unless it may open connections,
    /// binding unless it may be connected to.
    net_handled: u64,
    /// Its volumes' mount points (D109), and whether each is read-only.
    volumes: Vec<(CString, bool)>,
    /// The run's plug roots (D235), `/Users`, beneath which Landlock grants it every right.
    plug_roots: Vec<CString>,
    /// Its Unix sockets' directories it receives on, which Landlock lets it make sockets in.
    receives: Vec<CString>,
    /// Its seccomp filter, as the run gave it ([`Filter`]), and its length in instructions,
    /// within `sock_fprog`'s u16.
    seccomp: Filter,
    seccomp_len: u16,
    /// Its variables as it started, `KEY=VALUE`, which an exec's own go over.
    pub env: Vec<CString>,
    /// Its working directory as it started: an exec's where it names none.
    pub workdir: CString,
}

impl std::fmt::Debug for Confinement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Confinement")
            .field("id", &self.id)
            .field("volumes", &self.volumes.len())
            .field("plug_roots", &self.plug_roots.len())
            .field("receives", &self.receives.len())
            .field("seccomp", &self.seccomp_len)
            .finish_non_exhaustive()
    }
}

/// The step of [`confine`] that failed, its errno the last; numbered as a command run in a
/// member reports it ([`crate::enter`], `members::Refusal::Confining`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Confining {
    BoundingSet = 1,
    Ids,
    Capabilities,
    NoNewPrivs,
    Ruleset,
    BaseRule,
    VolumeOpen,
    VolumeRule,
    PlugRootRule,
    SocketRule,
    Landlock,
    Signals,
    Seccomp,
}

impl Confining {
    const ALL: [Confining; 13] = [
        Confining::BoundingSet,
        Confining::Ids,
        Confining::Capabilities,
        Confining::NoNewPrivs,
        Confining::Ruleset,
        Confining::BaseRule,
        Confining::VolumeOpen,
        Confining::VolumeRule,
        Confining::PlugRootRule,
        Confining::SocketRule,
        Confining::Landlock,
        Confining::Signals,
        Confining::Seccomp,
    ];

    /// The step numbered `n`, as [`Confining`]'s discriminants number them.
    pub fn numbered(n: u8) -> Option<Confining> {
        Confining::ALL.into_iter().find(|c| *c as u8 == n)
    }

    pub fn what(self) -> &'static CStr {
        match self {
            Confining::BoundingSet => c"dropping its capabilities",
            Confining::Ids => c"taking its own IDs",
            Confining::Capabilities => c"clearing its capabilities",
            Confining::NoNewPrivs => c"setting no_new_privs",
            Confining::Ruleset => c"making its Landlock ruleset",
            Confining::BaseRule => c"adding a Landlock rule",
            Confining::VolumeOpen => c"opening its volume for Landlock",
            Confining::VolumeRule => c"adding its volume's Landlock rule",
            Confining::PlugRootRule => c"adding a plug root's Landlock rule",
            Confining::SocketRule => c"adding a Landlock rule for its Unix sockets",
            Confining::Landlock => c"entering its Landlock ruleset",
            Confining::Signals => c"restoring its signals",
            Confining::Seccomp => c"loading its seccomp filter",
        }
    }
}

/// Confines this process as member `c` is, last before it execs: the member's own first
/// process, in its mounts, and a command run in it ([`crate::enter`]), in its namespaces. Calls
/// the kernel alone, on what `c` holds, for a child of `clone`; the first step that failed.
///
/// # Safety
/// In a process of one thread, which execs or exits next.
pub(crate) unsafe fn confine(c: &Confinement) -> Result<(), Confining> {
    // SAFETY: system calls on `c`'s buffers and this stack's, in a process of one thread.
    unsafe {
        // One that receives makes its sockets for others' uids to connect to (unix(7):
        // connecting needs write permission on the socket), and `bind` applies its umask
        // (net/unix/af_unix.c, `unix_bind_bsd`): none, whom the directory shows to only those
        // granted.
        if !c.receives.is_empty() {
            libc::umask(0);
        }
        // Its own IDs and no capability: the bounding set first, which needs CAP_SETPCAP;
        // the permitted and effective sets go as no uid stays 0 (capabilities(7), "Effect
        // of user ID changes on capabilities"); the inheritable and ambient sets cleared.
        if !crate::defaults::bound(c.last_cap, |_| false) {
            return Err(Confining::BoundingSet);
        }
        if !crate::defaults::take_ids(&[], c.id, c.id) {
            return Err(Confining::Ids);
        }
        if !crate::defaults::set(c.last_cap, |_| false)
            || libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0
        {
            return Err(Confining::Capabilities);
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(Confining::NoNewPrivs);
        }
        landlocked(c)?;
        // Its signals a new process's, as an exec's are: none ignored, which execve would
        // keep ignored (init's SIGPIPE), and none blocked.
        if !crate::defaults::signals() {
            return Err(Confining::Signals);
        }
        // Its seccomp filter, last: what it refuses, nothing after may need.
        let program = libc::sock_fprog {
            len: c.seccomp_len,
            filter: c.seccomp.1.as_ptr().cast_mut(),
        };
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            c.seccomp.0,
            &raw const program,
        ) != 0
        {
            return Err(Confining::Seccomp);
        }
        Ok(())
    }
}

/// Puts this process under member `c`'s Landlock ruleset, a layer under its mounts': every
/// filesystem right handled, and given back as reads and execution everywhere, writes to
/// /dev's nodes, and everything in its scratch, its writable volumes and beneath the run's
/// plug roots; the TCP binds and connects its links do not grant handled, and none given;
/// signals and abstract Unix sockets scoped to its domain. Each path as this process sees
/// it, in the member's mounts.
///
/// # Safety
/// As [`confine`].
unsafe fn landlocked(c: &Confinement) -> Result<(), Confining> {
    // SAFETY: Landlock's system calls on structs of this stack and the paths of `c`, open(2)
    // and close(2) of descriptors this process alone holds.
    unsafe {
        let attr = landlock::RulesetAttr {
            handled_access_fs: landlock::FS_ALL,
            handled_access_net: c.net_handled,
            scoped: landlock::SCOPE_ABSTRACT_UNIX_SOCKET | landlock::SCOPE_SIGNAL,
        };
        let ruleset = libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            std::mem::size_of::<landlock::RulesetAttr>(),
            0u32,
        );
        if ruleset < 0 {
            return Err(Confining::Ruleset);
        }
        // `fd` beneath which `allowed` is given, closed; whether the rule was added.
        let rule = |fd: libc::c_int, allowed: u64| {
            let rule = landlock::PathBeneathAttr {
                allowed_access: allowed,
                parent_fd: fd,
            };
            let added = libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                landlock::RULE_PATH_BENEATH,
                &raw const rule,
                0u32,
            );
            libc::close(fd);
            added == 0
        };
        let open =
            |at: &CStr, flags: libc::c_int| libc::open(at.as_ptr(), libc::O_PATH | libc::O_CLOEXEC | flags);
        let read = landlock::READ_FILE | landlock::READ_DIR | landlock::EXECUTE;
        let added = (|| {
            // A base path that is not there is skipped.
            for (path, allowed) in [
                (c"/", read),
                (c"/dev", read | landlock::WRITE_FILE | landlock::IOCTL_DEV),
                (c"/dev/shm", landlock::FS_ALL),
                (c"/dev/mqueue", landlock::FS_ALL),
                (c"/tmp", landlock::FS_ALL),
            ] {
                let fd = open(path, 0);
                if fd >= 0 && !rule(fd, allowed) {
                    return Err(Confining::BaseRule);
                }
            }
            // Its volumes, written as it pleases where they are not read-only (D109).
            for (at, ro) in &c.volumes {
                let fd = open(at, 0);
                if fd < 0 {
                    return Err(Confining::VolumeOpen);
                }
                if !rule(fd, if *ro { read } else { landlock::FS_ALL }) {
                    return Err(Confining::VolumeRule);
                }
            }
            // The run's plug roots (D235): what is plugged into it as it runs, beneath one,
            // written as it pleases where it is not read-only. A root grants nothing before:
            // it lies on its root's mount, read-only. One that is no directory in its view
            // is skipped, as the base paths above are.
            for at in &c.plug_roots {
                let fd = open(at, libc::O_DIRECTORY);
                if fd >= 0 && !rule(fd, landlock::FS_ALL) {
                    return Err(Confining::PlugRootRule);
                }
            }
            for at in &c.receives {
                let fd = open(at, 0);
                if fd < 0 || !rule(fd, landlock::MAKE_SOCK | landlock::REMOVE_FILE) {
                    return Err(Confining::SocketRule);
                }
            }
            Ok(())
        })();
        let entered = match added {
            Ok(()) if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0 => {
                Err(Confining::Landlock)
            }
            done => done,
        };
        libc::close(ruleset as libc::c_int);
        entered
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// Init's own check of the nesting grant against the image as built (D234): granted
    /// only where the normalized Agentfile declares `NESTING` (`"nesting": true`) and no
    /// agent or harness; each refusal in the words the build and the daemon share.
    #[test]
    fn init_refuses_the_grant_unless_the_image_as_built_declares_it_alone() {
        let lists = r#""skills":[],"mcp":[],"networks":[],"connections":[],"attachments":[],"exposures":[],"volumes":[]"#;
        let spec = |agents: &str, harnesses: &str, nesting: &str| {
            format!(r#"{{"schemaVersion":1,"agents":[{agents}],"harnesses":[{harnesses}],{lists}{nesting}}}"#)
        };
        let agent = r#"{"name":"a","source":"./a","to":"/agents/a","processes":null}"#;
        let harness = r#"{"name":"h","source":"./h","to":"/harness/h","processes":null}"#;
        assert_eq!(nesting_of(spec("", "", r#","nesting":true"#).as_bytes()), Ok(()));
        // As the build's spec() writes it (step 2): the grant after the schema's version.
        let built = format!(r#"{{"schemaVersion":1,"nesting":true,"agents":[],"harnesses":[],{lists}}}"#);
        assert_eq!(nesting_of(built.as_bytes()), Ok(()));
        for undeclared in [
            spec("", "", ""),
            spec("", "", r#","nesting":false"#),
            spec("", "", r#","nesting":"true""#),
            spec("", "", r#","nesting":1"#),
            spec("", "", r#","nesting":null"#),
            spec(agent, "", ""),
        ] {
            assert_eq!(
                nesting_of(undeclared.as_bytes()),
                Err(Nesting::Undeclared),
                "{undeclared}"
            );
        }
        assert_eq!(
            nesting_of(spec(agent, harness, r#","nesting":true"#).as_bytes()),
            Err(Nesting::Beside {
                kind: "agent",
                name: "a".into()
            })
        );
        assert_eq!(
            nesting_of(spec("", harness, r#","nesting":true"#).as_bytes()),
            Err(Nesting::Beside {
                kind: "harness",
                name: "h".into()
            })
        );
        assert!(matches!(
            nesting_of(br#"{"schemaVersion":2,"nesting":true}"#),
            Err(Nesting::Unread(_))
        ));
        assert!(matches!(nesting_of(b"{"), Err(Nesting::Unread(_))));
        assert_eq!(
            Nesting::Undeclared.to_string(),
            "cannot give the command the nesting grant: the image's Agentfile declares no NESTING"
        );
        assert_eq!(
            Nesting::Beside {
                kind: "harness",
                name: "h".into()
            }
            .to_string(),
            shards_abi::run::beside::nesting("harness", "h")
        );
    }

    /// A member's environment (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 7): init's
    /// own, then the image's ENV over it, but for its PATH, which init's holds after the
    /// member's own directories, and its HOME, the member's own; then its config's run.env,
    /// the last given of each name winning; never its domain's name, from either. What is
    /// staged for it goes over all of it (staged.rs).
    #[test]
    fn a_members_environment_is_inits_then_its_images_then_its_own() {
        let given = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            harness_env("/d", "harness agy", &[], &[]).unwrap(),
            [
                format!("PATH=/d:/d/bin:{PATH}"),
                "HOME=/tmp".into(),
                "SHARDS_DOMAIN=harness agy".into()
            ]
        );
        let image = given(&[
            "PATH=/usr/local/cargo/bin:/usr/bin",
            "HOME=/root",
            "LANG=C.UTF-8",
            "TZ=UTC",
        ]);
        assert_eq!(
            harness_env("/d", "harness agy", &image, &[]).unwrap(),
            [
                "PATH=/d:/d/bin:/usr/local/cargo/bin:/usr/bin",
                "HOME=/tmp",
                "SHARDS_DOMAIN=harness agy",
                "LANG=C.UTF-8",
                "TZ=UTC"
            ]
        );
        let env = harness_env(
            "/d",
            "harness agy",
            &image,
            &given(&["HOME=/home/agy", "LANG=C", "HOME=/home/two", "PATH=/opt/agy/bin"]),
        )
        .unwrap();
        assert_eq!(
            env,
            [
                "PATH=/opt/agy/bin",
                "HOME=/home/two",
                "SHARDS_DOMAIN=harness agy",
                "LANG=C",
                "TZ=UTC"
            ]
        );
        for (bad, why) in [
            ("SHARDS_DOMAIN=agent x", "SHARDS_DOMAIN"),
            ("NOEQUALS", "NAME=value"),
            ("=x", "NAME=value"),
        ] {
            let e = harness_env("/d", "harness agy", &[], &given(&[bad])).unwrap_err();
            assert!(e.contains(why) && e.contains("run.env"), "{bad}: {e}");
            let e = harness_env("/d", "harness agy", &given(&[bad]), &[]).unwrap_err();
            assert!(e.contains(why) && e.contains("the image's ENV"), "{bad}: {e}");
        }
    }

    /// A member's own working directory is its config's `run.workdir` in its directory, held
    /// to it as its build holds it: relative, never leaving it, however crafted.
    #[test]
    fn a_members_own_working_directory_stays_in_its_directory() {
        for good in [".", "work", "a/../b", "./x/", "a/b/.."] {
            assert!(inside(good), "{good}");
        }
        for bad in ["", "/etc", "..", "../h2", "a/../../etc", "a\0b"] {
            assert!(!inside(bad), "{bad:?}");
        }
        let config = |workdir: &str| {
            json::parse(format!(r#"{{"run":{{"command":["/bin/true"],"workdir":"{workdir}"}}}}"#).as_bytes())
                .unwrap()
        };
        let declared = json::parse(br#"{"name":"h"}"#).unwrap();
        let d = |workdir: &str| {
            let c = config(workdir);
            domain(
                "harness",
                "h",
                "/harness/h",
                FIRST_ID,
                &declared,
                &c,
                c.get("run").unwrap(),
            )
        };
        assert_eq!(
            d("work").unwrap().workdir,
            CString::new("/harness/h/work").unwrap()
        );
        let e = d("../../etc").unwrap_err();
        assert!(
            e.contains("must be relative and stay inside its directory"),
            "{e}"
        );
        assert!(d("/etc").is_err());
    }

    /// A member's HOME lies in the deepest of its writable mounts that holds it, however it
    /// is written but for `..`; what is staged for it goes there (staged.rs), never to a
    /// mount beside it.
    #[test]
    fn a_members_home_lies_in_the_deepest_mount_that_holds_it() {
        let points = ["/tmp", "/var/home/claude", "/home/claude", "/home/claude/.cache"];
        assert_eq!(home_in("/tmp", &points), Some((0, vec![])));
        assert_eq!(home_in("/tmp/h/i", &points), Some((0, vec!["h", "i"])));
        assert_eq!(home_in("/home/claude", &points), Some((2, vec![])));
        assert_eq!(home_in("//home/claude/./", &points), Some((2, vec![])));
        assert_eq!(home_in("/home/claude/.cache/x", &points), Some((3, vec!["x"])));
        assert_eq!(home_in("/var/home/claude/w", &points), Some((1, vec!["w"])));
        for nowhere in [
            "/root",
            "/home",
            "/home/claudette",
            "tmp",
            "",
            "/tmp/../etc",
            "/home/claude/../x",
        ] {
            assert_eq!(home_in(nowhere, &points), None, "{nowhere}");
        }
        assert_eq!(home_in("/tmp", &[]), None);
    }

    /// A domain of `argv` whose directory is `dir`.
    fn domain_at(dir: &str, argv: &[&str]) -> Domain {
        Domain {
            label: "agent a".into(),
            cgroup: "agent-a".into(),
            dir: dir.as_bytes().to_vec(),
            id: FIRST_ID,
            pids: None,
            no_processes: false,
            memory: None,
            argv: argv.iter().map(|a| CString::new(*a).unwrap()).collect(),
            run_env: Vec::new(),
            workdir: CString::default(),
            name: (false, "a".into()),
            link: None,
            channels: Vec::new(),
            mcp: "[]".into(),
            resolv: None,
            volumes: Vec::new(),
        }
    }

    /// A member's program: an absolute one as written, a relative one its directory's, a
    /// bare name found on the PATH it runs with, whatever gave that PATH last.
    #[test]
    fn a_members_program_is_found_on_the_path_it_runs_with() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = shards_testdir::TempDir::new("program").unwrap();
        let root = dir.path().to_str().unwrap();
        for (d, mode) in [("bin", 0o755), ("other", 0o755)] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
            let tool = dir.path().join(d).join("tool");
            std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let env = |path: String| vec![CString::new("HOME=/tmp").unwrap(), CString::new(path).unwrap()];
        let found = |argv: &[&str], path: String| executable(&domain_at(root, argv), &env(path));
        let at = |p: &str| CString::new(format!("{root}/{p}")).unwrap();
        assert_eq!(
            found(&["tool"], format!("PATH={root}/none:{root}/bin")).unwrap(),
            at("bin/tool")
        );
        assert_eq!(
            found(&["tool"], format!("PATH={root}/other:{root}/bin")).unwrap(),
            at("other/tool")
        );
        assert_eq!(
            found(&["bin/tool", "-x"], "PATH=/nowhere".into()).unwrap(),
            at("bin/tool")
        );
        assert_eq!(
            found(&["/usr/bin/env"], "PATH=/nowhere".into()).unwrap(),
            CString::new("/usr/bin/env").unwrap()
        );
        let e = found(&["tool"], "PATH=/nowhere".into()).unwrap_err();
        assert!(e.contains("\"tool\" is on no directory of PATH"), "{e}");
    }

    /// A member's command line (D256): its program as found, the rest of its config's
    /// `run.command` (its entrypoint's words, then its options), then its run's arguments in
    /// their order, an empty one among them; none staged, its config's alone, as before.
    #[test]
    fn a_members_command_line_is_its_command_then_its_runs_arguments() {
        let d = domain_at(
            "/agents/claude",
            &["claude", "-p", "--output-format", "stream-json"],
        );
        let program = CString::new("/usr/local/bin/claude").unwrap();
        let line = |args: &[CString]| -> Vec<Vec<u8>> {
            command_line(&program, &d, args)
                .iter()
                .map(|a| a.to_bytes().to_vec())
                .collect()
        };
        let own = [
            &b"/usr/local/bin/claude"[..],
            b"-p",
            b"--output-format",
            b"stream-json",
        ];
        assert_eq!(line(&[]), own);
        let given: Vec<CString> = ["--resume", "s-1", "", "--model"]
            .iter()
            .map(|a| CString::new(*a).unwrap())
            .collect();
        assert_eq!(
            line(&given),
            [&own[..], &[&b"--resume"[..], b"s-1", b"", b"--model"][..]].concat()
        );
    }

    /// The host's entries (volumes.rs, `open`): those for the domains, with their tag,
    /// cleaned mount point and flags; a file bound alone, and the run's own, left out.
    #[test]
    fn the_runs_shares_for_its_domains_are_read_from_its_entries() {
        let setup: Vec<Vec<u8>> = vec![
            b"volume=shards0\0/data//\0copy,domains,own\0".to_vec(),
            b"domain-volume=shards1\0/secret\0ro,domains\0".to_vec(),
            b"volume=shards2\0/run-own\0copy\0".to_vec(),
            b"volume=shards3\0/etc/one.conf\0domains\0one.conf".to_vec(),
            b"tmpfs=/t\0rw".to_vec(),
        ];
        assert_eq!(
            shares_of(&setup),
            vec![
                Share {
                    tag: "shards0".into(),
                    dest: "/data".into(),
                    ro: false,
                    copy: true,
                    main: true,
                    own: true,
                },
                Share {
                    tag: "shards1".into(),
                    dest: "/secret".into(),
                    ro: true,
                    copy: false,
                    main: false,
                    own: false,
                },
            ]
        );
    }

    /// A volume no `VOLUME` scopes is every domain's; one `FOR` scopes, those it names.
    #[test]
    fn a_volume_is_given_as_its_volume_says() {
        let d = |volumes: Vec<VolumeRule>| Domain {
            volumes,
            ..domain_at("/agents/a", &[])
        };
        let rule = |path: &str, granted: bool| VolumeRule {
            path: path.into(),
            granted,
            chown: String::new(),
            chmod: String::new(),
        };
        assert!(given_volume(&d(vec![]), "/docker-own"));
        assert!(given_volume(&d(vec![rule("/mine", true)]), "/mine"));
        assert!(!given_volume(&d(vec![rule("/theirs", false)]), "/theirs"));
        assert!(given_volume(&d(vec![rule("/theirs", false)]), "/other"));
    }

    /// One volume at a mount point, however its path is written.
    #[test]
    fn two_volumes_never_share_a_mount_point() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().into_iter();
        assert!(once_each(s(&["/a", "/b", "/a/b"])).is_ok());
        for paths in [
            &["/a", "/a"][..],
            &["/a", "/a/"],
            &["/a", "a"],
            &["/a/../b", "/b"],
        ] {
            assert!(once_each(s(paths)).is_err(), "{paths:?}");
        }
    }

    /// Runs `work` in a child of the test's, as root of a mount namespace of its own, and
    /// returns what it says: in a user namespace of its own where the test is not root
    /// already, its uid and gid there 0 (user_namespaces(7)), standing in for init's
    /// privileges. None, said as a SKIP, where this host gives an unprivileged process
    /// neither namespace.
    fn as_root_of_own_mounts(work: impl FnOnce() -> String) -> Option<String> {
        use std::io::{Read as _, Write as _};
        let mut fds = [0 as RawFd; 2];
        // SAFETY: pipe2(2) into a two-element array.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors were just made, and are owned here alone.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: getuid(2), getgid(2) and geteuid(2) cannot fail.
        let (uid, gid, root) = unsafe { (libc::getuid(), libc::getgid(), libc::geteuid() == 0) };
        // SAFETY: fork(2); the child runs `work` and _exit(2)s, never returning into the
        // test harness.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            drop(r);
            // A panic here is the child's to say, never to unwind into the harness.
            let said = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let flags = libc::CLONE_NEWNS | if root { 0 } else { libc::CLONE_NEWUSER };
                // SAFETY: unshare(2) of this single-threaded child.
                if unsafe { libc::unshare(flags) } != 0 {
                    return format!("SKIP: unshare: {}", io::Error::last_os_error());
                }
                if !root {
                    let maps = std::fs::write("/proc/self/setgroups", "deny")
                        .and_then(|()| std::fs::write("/proc/self/uid_map", format!("0 {uid} 1")))
                        .and_then(|()| std::fs::write("/proc/self/gid_map", format!("0 {gid} 1")));
                    if let Err(e) = maps {
                        return format!("SKIP: its ID maps: {e}");
                    }
                }
                // SAFETY: mount(2) of NUL-terminated literals, in this child's own namespace.
                if unsafe {
                    libc::mount(
                        std::ptr::null(),
                        c"/".as_ptr(),
                        std::ptr::null(),
                        libc::MS_REC | libc::MS_PRIVATE,
                        std::ptr::null(),
                    )
                } != 0
                {
                    return format!("SKIP: its mounts private: {}", io::Error::last_os_error());
                }
                work()
            }))
            .unwrap_or_else(|_| "the child panicked".into());
            let _ = std::fs::File::from(w).write_all(said.as_bytes());
            // SAFETY: ends the child without running the harness's exit paths.
            unsafe { libc::_exit(0) }
        }
        drop(w);
        let mut said = String::new();
        std::fs::File::from(r).read_to_string(&mut said).unwrap();
        // SAFETY: waits for the test's own child.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        if said.starts_with("SKIP") {
            eprintln!("{said}");
            return None;
        }
        Some(said)
    }

    /// A domain's root shows each directory the image has as the image made it, where its
    /// skeleton makes one there too: a volume's mount point's parents, `/etc`, the root
    /// itself. overlayfs shows a merged directory with its upper layer's owner and mode, and
    /// the skeleton's were root's, 0755, whatever the image's
    /// (docs/research/measurements/s7-home-workdir). Owners are checked where the test runs
    /// as root of IDs past its own (`unshare --map-auto --map-root-user`).
    #[test]
    fn a_skeletons_directories_are_the_images_own() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let dir = shards_testdir::TempDir::new("skel").unwrap();
        let (lower, at) = (dir.path().join("lower"), dir.path().join("at"));
        for d in ["home/node", "etc", "tmp"] {
            std::fs::create_dir_all(lower.join(d)).unwrap();
        }
        std::fs::create_dir(&at).unwrap();
        for (d, mode) in [("", 0o751), ("home/node", 0o750), ("etc", 0o711), ("tmp", 0o1777)] {
            std::fs::set_permissions(lower.join(d), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let said = as_root_of_own_mounts(|| {
            // Another user's, where this namespace has one (1000), to see owners kept too.
            let owners = std::os::unix::fs::chown(lower.join("home/node"), Some(1000), Some(1000)).is_ok();
            let image = std::fs::File::open(&lower).unwrap();
            let skel = match skeleton(image.as_fd(), &[c"home/node/.cache".into()], &[]) {
                Ok(s) => s,
                Err(e) => return format!("skeleton: {e}"),
            };
            let root = match image_root(&skel, image.as_fd(), false) {
                Ok(r) => r,
                Err(e) => return format!("its root: {e}"),
            };
            let target = CString::new(at.to_str().unwrap()).unwrap();
            // SAFETY: move_mount(2) of the detached root onto a NUL-terminated path.
            if unsafe {
                libc::syscall(
                    libc::SYS_move_mount,
                    root.as_raw_fd(),
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    target.as_ptr(),
                    mount_api::MOVE_MOUNT_F_EMPTY_PATH,
                )
            } != 0
            {
                return format!("attaching its root: {}", io::Error::last_os_error());
            }
            let mut out = String::new();
            for d in ["", "home", "home/node", "etc", "tmp", "home/node/.cache"] {
                let (theirs, ours) = (lower.join(d), at.join(d));
                let shown = |p: &std::path::Path| {
                    std::fs::symlink_metadata(p).map_or_else(
                        |e| format!("{e}"),
                        |m| {
                            let who = if owners {
                                format!("{}:{} ", m.uid(), m.gid())
                            } else {
                                String::new()
                            };
                            format!("{who}{:o}", m.mode() & 0o7777)
                        },
                    )
                };
                out += &format!("/{d} {} as {};", shown(&ours), shown(&theirs));
            }
            out
        });
        let Some(said) = said else { return };
        eprintln!("{said}");
        let seen: Vec<(&str, &str, &str)> = said
            .split(';')
            .filter_map(|l| {
                let (d, rest) = l.split_once(' ')?;
                let (ours, theirs) = rest.split_once(" as ")?;
                Some((d, ours, theirs))
            })
            .collect();
        assert_eq!(seen.len(), 6, "{said}");
        for (d, ours, theirs) in seen {
            if d == "/home/node/.cache" {
                // The image lacks the mount point itself: the skeleton's own, root's, 0755.
                assert!(ours.ends_with("755") && theirs.contains("No such file"), "{said}");
            } else {
                assert_eq!(ours, theirs, "{d}: {said}");
            }
        }
    }

    /// A plug root (D235) grants nothing until something is plugged there: Landlock grants
    /// every right beneath it, and it lies on its domain's root, which the domain's mount of
    /// the microVM's system keeps read-only, so the domain can make nothing beneath it.
    #[test]
    fn a_plug_root_lies_on_a_read_only_root() {
        assert_ne!(SYSTEM & libc::MOUNT_ATTR_RDONLY, 0);
    }

    #[test]
    fn within_is_a_directory_and_what_lies_beneath() {
        assert!(within("/proc", "/proc"));
        assert!(within("/proc/self", "/proc"));
        assert!(!within("/process", "/proc"));
        assert!(!within("/", "/proc"));
    }

    /// A volume is given no domain where it would lie in what init mounts for each: its
    /// root and system, or its scratch, where a volume beneath finds no mount point and one
    /// at it hides the scratch (docs/research/measurements/s7-scratch-volume). The
    /// Agentfile's own are refused there, a Docker `VOLUME` an image's symlink leads there
    /// is the run's alone, as the build and the daemon hold them.
    #[test]
    fn no_volume_lies_in_what_init_mounts_for_a_domain() {
        for p in ["/", "/proc", "/sys/fs", "/dev", "/dev/shm"] {
            assert_eq!(domain_mount(p), Some(DomainMount::System), "{p}");
        }
        for p in ["/tmp", "/tmp/x", "/tmp/a/b"] {
            assert_eq!(domain_mount(p), Some(DomainMount::Scratch), "{p}");
        }
        for p in ["/data", "/tmpfiles", "/var/tmp", "/home/h/tmp"] {
            assert_eq!(domain_mount(p), None, "{p}");
        }
        assert_eq!(given_at("/data", "/data", true), Ok(true));
        assert_eq!(given_at("/var/run/x", "/run/x", false), Ok(true));
        for declared in [false, true] {
            assert_eq!(given_at("/tmp/x", "/tmp/x", declared), Err(DomainMount::Scratch));
            assert_eq!(given_at("/tmp", "/tmp", declared), Err(DomainMount::Scratch));
            assert_eq!(
                given_at("/dev/shm", "/dev/shm", declared),
                Err(DomainMount::System)
            );
        }
        // Led into the scratch by the image's symlink (`/cache` -> `/tmp/cache`).
        assert_eq!(given_at("/cache", "/tmp/cache", false), Ok(false));
        assert_eq!(given_at("/cache", "/tmp/cache", true), Err(DomainMount::Scratch));
        assert_eq!(given_at("/var/shm", "/dev/shm", false), Ok(false));
        assert!(
            DomainMount::Scratch
                .to_string()
                .contains("/tmp, a domain's own scratch"),
            "{}",
            DomainMount::Scratch
        );
    }
}
