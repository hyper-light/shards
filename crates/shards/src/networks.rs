//! User-defined networks (D46), as dockerd's libnetwork keeps them (moby docker-v29.3.1 at
//! f78c987a: daemon/network.go, daemon/libnetwork network.go and ipams/defaultipam), in
//! the home (`networks/ID.json`), and their addresses: each network's IPv4 pool from
//! dockerd's default pools, the lowest that overlaps no other network's nor the host's
//! (InferReservedNetworks), unless one is asked for; its gateway the pool's (or its
//! range's) first free address; each member's the lowest free one, in its range where it
//! has one. dockerd's predefined `bridge`, `host` and `none` are listed beside them.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shards_net::bridge::{Prefix, free_subnet, overlap};

/// dockerd's predefined networks, which no user's may be named or removed.
pub const PREDEFINED: [&str; 4] = ["bridge", "host", "none", "default"];

/// An IPv4 pool of a network: its subnet, the range its members' addresses come from,
/// its gateway and its auxiliary addresses (IPAM.Config's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub subnet: Prefix,
    #[serde(default)]
    pub ip_range: Option<Prefix>,
    pub gateway: Ipv4Addr,
    #[serde(default)]
    pub aux: BTreeMap<String, Ipv4Addr>,
}

/// An IPv6 pool, kept as given or allocated, for what inspect says: shards' guests have no
/// IPv6 (D31), so no member takes an address of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool6 {
    pub subnet: String,
    #[serde(default)]
    pub ip_range: String,
    pub gateway: String,
    #[serde(default)]
    pub aux: BTreeMap<String, String>,
}

/// A network, as dockerd's network.Inspect says one, less its members, which its runs
/// hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    pub id: String,
    /// When it was made, in nanoseconds since the epoch.
    pub created: u128,
    pub scope: String,
    pub driver: String,
    pub ipv4: bool,
    pub ipv6: bool,
    pub ipam_driver: String,
    #[serde(default)]
    pub ipam_options: BTreeMap<String, String>,
    #[serde(default)]
    pub pools: Vec<Pool>,
    #[serde(default)]
    pub pools6: Vec<Pool6>,
    /// The pools given, IPv6's first where only IPv6's were (buildIPAMResources: the
    /// user's of each family first, then those allocated).
    #[serde(default)]
    pub v6_first: bool,
    pub internal: bool,
    pub attachable: bool,
    pub ingress: bool,
    #[serde(default)]
    pub config_from: String,
    pub config_only: bool,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Network {
    /// Whether it is one of dockerd's predefined ones.
    pub fn predefined(&self) -> bool {
        PREDEFINED.contains(&self.name.as_str())
    }
}

/// The networks in the home, each its own file.
pub struct Store {
    root: PathBuf,
}

/// One network change at a time: a create's pool and name checks hold until it is kept.
static CHANGES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds network changes off until it is dropped.
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    CHANGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Store {
    pub fn new(home: &Path) -> Store {
        Store {
            root: home.join("networks"),
        }
    }

    /// Every network kept, in no order.
    pub fn list(&self) -> Vec<Network> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".json")))
            .filter_map(|e| std::fs::read(e.path()).ok())
            .filter_map(|b| serde_json::from_slice(&b).ok())
            .collect()
    }

    /// Keeps `n`, whole or not at all.
    pub fn put(&self, n: &Network) -> Result<(), String> {
        make_dir(&self.root).map_err(|e| format!("network {}: {e}", n.name))?;
        let text = serde_json::to_vec(n).map_err(|e| e.to_string())?;
        let tmp = self.root.join(format!(".{}.json", n.id));
        std::fs::write(&tmp, &text).map_err(|e| format!("network {}: {e}", n.name))?;
        std::fs::rename(&tmp, self.root.join(format!("{}.json", n.id)))
            .map_err(|e| format!("network {}: {e}", n.name))
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        std::fs::remove_file(self.root.join(format!("{id}.json"))).map_err(|e| e.to_string())
    }
}

/// `dir`, made if it is not there, in a home that must be: a home that has been removed
/// is never made again by what writes in it (a daemon going on writing as it ends).
pub fn make_dir(dir: &std::path::Path) -> std::io::Result<()> {
    match std::fs::create_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        r => r,
    }
}

/// How a network's name or ID prefix resolves (daemon FindNetwork and its kin): a full
/// ID, then an exact name, then one ID prefix.
pub enum Found<'a> {
    One(&'a Network),
    /// Matched by name, or by ID prefix, more than once: how many, and which way.
    Ambiguous(usize, bool),
    None,
}

pub fn find<'a>(networks: &'a [Network], term: &str) -> Found<'a> {
    if let Some(n) = networks.iter().find(|n| n.id == term) {
        return Found::One(n);
    }
    let named: Vec<&Network> = networks.iter().filter(|n| n.name == term).collect();
    match named.as_slice() {
        [one] => return Found::One(one),
        [_, _, ..] => return Found::Ambiguous(named.len(), true),
        [] => {}
    }
    let prefixed: Vec<&Network> = networks
        .iter()
        .filter(|n| !term.is_empty() && n.id.starts_with(term))
        .collect();
    match prefixed.as_slice() {
        [one] => Found::One(one),
        [_, _, ..] => Found::Ambiguous(prefixed.len(), false),
        [] => Found::None,
    }
}

/// The mask of a prefix `bits` long.
fn mask(bits: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(bits.min(32))).unwrap_or(0)
}

/// Whether `ip` is in `p`.
pub fn contains(p: Prefix, ip: Ipv4Addr) -> bool {
    u32::from(ip) & mask(p.1) == u32::from(p.0) & mask(p.1)
}

/// The addresses a pool reserves of itself (newPoolData): its network's and broadcast's,
/// where it has more than two.
fn reserved_of(p: Prefix) -> Vec<Ipv4Addr> {
    if 32 - p.1 <= 1 {
        return Vec::new();
    }
    let base = u32::from(p.0) & mask(p.1);
    vec![Ipv4Addr::from(base), Ipv4Addr::from(base | !mask(p.1))]
}

/// Where in `pool` an address comes from: its range, or its whole subnet.
fn span(pool: &Pool) -> Prefix {
    pool.ip_range.unwrap_or(pool.subnet)
}

/// What of `pool` is in use: its reservations, gateway and auxiliary addresses, and
/// `members`'.
pub fn in_use(pool: &Pool, members: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut used = reserved_of(pool.subnet);
    used.push(pool.gateway);
    used.extend(pool.aux.values().copied());
    used.extend(members.iter().copied().filter(|ip| contains(pool.subnet, *ip)));
    used.sort_unstable();
    used.dedup();
    used
}

/// The lowest address of `within` that `used` lacks.
fn lowest_free(within: Prefix, used: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    let base = u64::from(u32::from(within.0) & mask(within.1));
    let size = 1u64 << (32 - u32::from(within.1));
    (base..base + size)
        .filter_map(|a| u32::try_from(a).ok())
        .map(Ipv4Addr::from)
        .find(|ip| !used.contains(ip))
}

/// A member's address in `pool`, the lowest free in its span (libnetwork's local IPAM
/// takes the lowest, not the next: a freed address is the next taken).
pub fn allocate(pool: &Pool, members: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    lowest_free(span(pool), &in_use(pool, members))
}

/// Status.IPAM.Subnets' counts for `pool`: every address marked in it, and those of its
/// span still free.
pub fn counts(pool: &Pool, members: &[Ipv4Addr]) -> (u64, u64) {
    let used = in_use(pool, members);
    let span = span(pool);
    let size = 1u64 << (32 - u32::from(span.1));
    let marked_in_span = used.iter().filter(|ip| contains(span, **ip)).count() as u64;
    (used.len() as u64, size.saturating_sub(marked_in_span))
}

/// A network's IPv4 pool as it is created: the subnet asked for, or the lowest of
/// dockerd's pools that overlaps none of `taken` (`--subnet 0.0.0.0/BITS` asks for one
/// that long); its range, gateway and auxiliary addresses checked as libnetwork checks
/// them. dockerd's words when it fails.
pub fn new_pool(
    asked: Option<Prefix>,
    ip_range: Option<Prefix>,
    gateway: Option<Ipv4Addr>,
    aux: &BTreeMap<String, Ipv4Addr>,
    taken: &[Prefix],
) -> Result<Pool, String> {
    let subnet = match asked {
        Some((addr, bits)) if addr.is_unspecified() => {
            free_subnet(taken, Some(bits)).ok_or("invalid address pool")?
        }
        Some(p) => {
            if taken.iter().any(|t| overlap(*t, p)) {
                return Err(
                    "invalid pool request: Pool overlaps with other one on this address space".into(),
                );
            }
            p
        }
        None => free_subnet(taken, None).ok_or("all predefined address pools have been fully subnetted")?,
    };
    let reserved = reserved_of(subnet);
    let mut pool = Pool {
        subnet,
        ip_range,
        gateway: Ipv4Addr::UNSPECIFIED,
        aux: BTreeMap::new(),
    };
    pool.gateway = match gateway {
        Some(gw) => {
            if reserved.contains(&gw) {
                return Err(format!(
                    "failed to allocate gateway ({gw}): Address already in use"
                ));
            }
            gw
        }
        None => lowest_free(span(&pool), &reserved).ok_or("no available addresses for the gateway")?,
    };
    for (name, ip) in aux {
        if *ip == pool.gateway || reserved.contains(ip) || pool.aux.values().any(|a| a == ip) {
            return Err(format!(
                "failed to allocate secondary ip address ({name}:{ip}): Address already in use"
            ));
        }
        pool.aux.insert(name.clone(), *ip);
    }
    Ok(pool)
}

/// A prefix as `ADDR/BITS`.
pub fn show(p: Prefix) -> String {
    format!("{}/{}", p.0, p.1)
}

/// `ADDR/BITS`, IPv4.
pub fn parse_prefix(s: &str) -> Option<Prefix> {
    let (addr, bits) = s.split_once('/')?;
    let bits: u8 = bits.parse().ok().filter(|b| *b <= 32)?;
    Some((addr.parse().ok()?, bits))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Prefix {
        parse_prefix(s).unwrap()
    }

    /// Docker Engine 29.3.1's own answers (shards-dind): pools taken lowest first past
    /// the host's and the bridge's, a freed one reused; gateways the span's first free
    /// address; members' the lowest free; and Status' counts.
    #[test]
    fn addresses_are_given_as_dockers_ipam_gives_them() {
        let none = BTreeMap::new();
        // The host on 172.17/16, the bridge on 172.18/16: the next is 172.19.
        let taken = [p("172.17.0.0/16"), p("172.18.0.0/16")];
        let pool = new_pool(None, None, None, &none, &taken).unwrap();
        assert_eq!(
            (show(pool.subnet), pool.gateway),
            ("172.19.0.0/16".into(), Ipv4Addr::new(172, 19, 0, 1))
        );
        assert_eq!(counts(&pool, &[]), (3, 65533));
        let first = allocate(&pool, &[]).unwrap();
        assert_eq!(first, Ipv4Addr::new(172, 19, 0, 2));
        assert_eq!(counts(&pool, &[first]), (4, 65532));
        // A /24 user subnet partly in 192.168.0.0/20 makes the next /20 the lowest free.
        let mut all: Vec<Prefix> = (17..=31).map(|b| (Ipv4Addr::new(172, b, 0, 0), 16)).collect();
        all.push(p("192.168.5.0/24"));
        assert_eq!(free_subnet(&all, None), Some(p("192.168.16.0/20")));
        // A range's first address is the gateway; its count is the range's.
        let range = new_pool(
            Some(p("10.77.0.0/24")),
            Some(p("10.77.0.128/25")),
            None,
            &none,
            &[],
        )
        .unwrap();
        assert_eq!(range.gateway, Ipv4Addr::new(10, 77, 0, 128));
        assert_eq!(allocate(&range, &[]), Some(Ipv4Addr::new(10, 77, 0, 129)));
        let r16 = new_pool(Some(p("10.13.0.0/16")), Some(p("10.13.5.0/24")), None, &none, &[]).unwrap();
        assert_eq!(r16.gateway, Ipv4Addr::new(10, 13, 5, 0));
        assert_eq!(
            counts(
                &new_pool(Some(p("10.1.0.0/24")), None, None, &none, &[]).unwrap(),
                &[]
            ),
            (3, 253)
        );
        assert_eq!(
            counts(
                &new_pool(Some(p("10.2.0.0/30")), None, None, &none, &[]).unwrap(),
                &[]
            ),
            (3, 1)
        );
        assert_eq!(
            counts(
                &new_pool(Some(p("10.3.0.0/31")), None, None, &none, &[]).unwrap(),
                &[]
            ),
            (1, 1)
        );
        assert_eq!(
            counts(
                &new_pool(Some(p("10.4.0.0/32")), None, None, &none, &[]).unwrap(),
                &[]
            ),
            (1, 0)
        );
        // Refusals, in dockerd's words.
        assert_eq!(
            new_pool(Some(p("172.19.0.0/16")), None, None, &none, &[p("172.19.0.0/16")]).unwrap_err(),
            "invalid pool request: Pool overlaps with other one on this address space"
        );
        assert_eq!(
            new_pool(
                Some(p("10.10.0.0/16")),
                None,
                Some(Ipv4Addr::new(10, 10, 0, 0)),
                &none,
                &[]
            )
            .unwrap_err(),
            "failed to allocate gateway (10.10.0.0): Address already in use"
        );
        let aux = BTreeMap::from([("gw".to_string(), Ipv4Addr::new(10, 10, 0, 1))]);
        assert_eq!(
            new_pool(Some(p("10.10.0.0/16")), None, None, &aux, &[]).unwrap_err(),
            "failed to allocate secondary ip address (gw:10.10.0.1): Address already in use"
        );
        // `--subnet 0.0.0.0/24`: a /24 of the pools.
        assert_eq!(
            new_pool(Some(p("0.0.0.0/24")), None, None, &none, &all)
                .unwrap()
                .subnet,
            p("192.168.0.0/24")
        );
    }
}
