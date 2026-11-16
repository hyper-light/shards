//! A network made and removed (D46, D285 slice 6): `network create`'s path, docker/cli's
//! checks of what it is asked (createIPAMConfig, consolidateIpam) then dockerd's
//! (daemon/network.go NetworkCreate, Controller.NewNetwork), each in its order and words, its
//! record kept under the home's networks lock; and a network's removal, its leases then its
//! record, refused while any process's run holds it. One path for each of its callers: the
//! daemon's `network create` and `network rm` (the Engine API's through them), and an
//! orchestrator's, which no daemon serves (`shards api-networks`, shards-api's `Networks`).

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use shards_net::bridge::{Bridge, Prefix};

use super::leases::{self, Leases};
use super::{Network, Pool, Pool6, Store};

/// What a network is asked to be made with, as `network create`'s flags give it: each list and
/// map as given, a family's flag where it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Create {
    pub name: String,
    pub subnets: Vec<String>,
    pub ranges: Vec<String>,
    pub gateways: Vec<String>,
    pub aux: BTreeMap<String, String>,
    /// Its driver: none, the bridge's.
    pub driver: String,
    pub options: BTreeMap<String, String>,
    /// `--ipv4` and `--ipv6` where given, which their options then do not decide.
    pub ipv4: Option<bool>,
    pub ipv6: Option<bool>,
    pub config_from: String,
    pub config_only: bool,
    pub scope: String,
    pub ipam_driver: String,
    pub ipam_options: BTreeMap<String, String>,
    pub internal: bool,
    pub attachable: bool,
    pub ingress: bool,
    pub labels: BTreeMap<String, String>,
}

impl Create {
    /// Network `name` as `network create NAME` makes one: a bridge network of IPv4 alone, its
    /// subnet the lowest of dockerd's pools that meets no other network's nor the host's.
    pub fn named(name: &str) -> Create {
        Create {
            name: name.to_string(),
            ..Create::default()
        }
    }
}

/// Why no network was made (R04): callers match the cause; its words are `network create`'s,
/// dockerd's where it has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmade {
    /// Its flags, as docker/cli refuses them before it asks (createIPAMConfig): said as the
    /// CLI says them, never as the daemon's.
    Flags,
    /// A name no user's network takes: none, or one of shards' own ([`super::PREDEFINED`]).
    Name,
    /// A network is named so already.
    Exists,
    /// Its settings: its driver, scope, families, options, its subnets' shapes, or the network
    /// it takes them from.
    Settings,
    /// Its pool: the subnet asked meets another network's, or none of dockerd's pools is free
    /// of the networks' and the host's; its gateway or an auxiliary address taken.
    Pool,
    /// The home's files or its networks lock refused, or the host's random source, which
    /// makes its ID.
    Host,
}

/// Why a network was not removed (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// No network is named so.
    Unknown,
    /// One of shards' own networks, which stay.
    Shards,
    /// A run is on it, of any process (a lease of it lives: the daemon's runs hold theirs as
    /// an orchestrator's run hosts do), a process links its runs, or networks take their
    /// settings from it.
    InUse,
    /// The home's files or its networks lock refused.
    Host,
}

/// The home's networks: dockerd's predefined ones, made here the first time they are asked
/// for, `bridge`'s on `bridge`'s subnet (the default network's, which its caller elected,
/// D31), then the user's.
pub fn all(home: &Path, bridge: Option<Bridge>) -> Vec<Network> {
    let store = Store::new(home);
    let mut all = store.list();
    for (name, driver) in [("bridge", "bridge"), ("host", "host"), ("none", "null")] {
        let pools: Vec<Pool> = if name == "bridge" {
            bridge
                .iter()
                .map(|b| Pool {
                    subnet: b.subnet(),
                    ip_range: None,
                    gateway: b.gateway(),
                    aux: BTreeMap::new(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let options = if name == "bridge" {
            bridge_options()
        } else {
            BTreeMap::new()
        };
        match all.iter_mut().find(|n| n.name == name) {
            // A home's record of what the default network does, kept so as shards
            // changes (the options a home of an older shards recorded).
            Some(n) if n.pools != pools || n.options != options => {
                n.pools = pools;
                n.options = options;
                let _ = store.put(n);
            }
            Some(_) => {}
            None => {
                let Ok(id) = crate::containers::new_id() else {
                    continue;
                };
                let n = Network {
                    name: name.into(),
                    id,
                    created: crate::containers::now(),
                    scope: "local".into(),
                    driver: driver.into(),
                    ipv4: true,
                    ipv6: false,
                    ipam_driver: "default".into(),
                    ipam_options: BTreeMap::new(),
                    pools,
                    pools6: Vec::new(),
                    v6_first: false,
                    internal: false,
                    attachable: false,
                    ingress: false,
                    config_from: String::new(),
                    config_only: false,
                    options,
                    labels: BTreeMap::new(),
                };
                let _ = store.put(&n);
                all.push(n);
            }
        }
    }
    all
}

/// Network `ask.name` made in `home` as `network create` makes one (NetworkCreate,
/// Controller.NewNetwork): docker/cli's checks, then dockerd's, each in its order and words;
/// its name and pool checked against every network kept ([`all`], `bridge` the default
/// network's), and kept, under the home's networks lock, which every process making a
/// network or leasing of one takes (D285). Its record, once kept.
pub fn create(home: &Path, bridge: Option<Bridge>, ask: &Create) -> Result<Network, (Unmade, String)> {
    let name = &ask.name;
    let configs =
        consolidate(&ask.subnets, &ask.ranges, &ask.gateways, &ask.aux).map_err(|e| (Unmade::Flags, e))?;
    let settings = |why: String| Err((Unmade::Settings, why));
    if super::PREDEFINED.contains(&name.as_str()) {
        return Err((
            Unmade::Name,
            format!("{name} is one of shards' own networks, bridge, host and none: give yours another name"),
        ));
    }
    let driver = match ask.driver.as_str() {
        "" => "bridge",
        d => d,
    };
    if driver == "overlay" {
        return settings(
            "--driver overlay: shards has no swarm mode, whose networks overlay ones are".into(),
        );
    }
    let mut options = ask.options.clone();
    let family =
        |given: Option<bool>, option: &str, default: bool, options: &mut BTreeMap<String, String>| {
            if let Some(given) = given {
                options.remove(option);
                return Ok(given);
            }
            match options.get(option) {
                Some(v) => shards_cmdline::go::parse_bool(v).map_err(|_| {
                    (
                        Unmade::Settings,
                        format!(
                            "driver-opt {} is not a valid bool",
                            shards_cmdline::go::quote(option)
                        ),
                    )
                }),
                None => Ok(default),
            }
        };
    let ipv4 = family(ask.ipv4, "com.docker.network.enable_ipv4", true, &mut options)?;
    let ipv6 = family(ask.ipv6, "com.docker.network.enable_ipv6", false, &mut options)?;
    // IPv6's configs go where IPv6 is off.
    let configs: Vec<Asked> = configs
        .into_iter()
        .filter(|c| ipv6 || netip_prefix(&c.subnet).is_ok_and(|(ip, _)| ip.is_ipv4()))
        .collect();
    let errs: Vec<String> = configs.iter().flat_map(ipam_errors).collect();
    if !errs.is_empty() {
        return settings(errs.join("\n"));
    }
    if name.trim().is_empty() {
        return Err((Unmade::Name, "a network needs a name".into()));
    }
    // Its name and pool checked against every network kept, and kept, under the home's
    // networks lock, which any process making a network takes (D285).
    let _held = leases::hold(home).map_err(|e| (Unmade::Host, e.to_string()))?;
    let networks = all(home, bridge);
    if networks.iter().any(|n| &n.name == name) {
        return Err((Unmade::Exists, format!("a network is named {name} already")));
    }
    let config_only = ask.config_only;
    let (driver, scope) = if config_only {
        ("null", "local".to_string())
    } else {
        (driver, ask.scope.clone())
    };
    let config_from = &ask.config_from;
    let mut from: Option<Network> = None;
    if !config_from.is_empty() {
        let Some(source) = networks.iter().find(|n| &n.name == config_from && n.config_only) else {
            return settings(format!(
                "--config-from {config_from}: no config-only network is named so"
            ));
        };
        if config_only {
            return settings(
                "--config-from with --config-only: a config-only network takes its settings from none".into(),
            );
        }
        if !configs.is_empty() {
            return settings(format!(
                "--config-from {config_from}: the network's subnets come from it: give none of your own"
            ));
        }
        if !options.is_empty() {
            return settings(format!(
                "--config-from {config_from}: the network's options come from it: give none of your own"
            ));
        }
        from = Some(source.clone());
    }
    match driver {
        "bridge" | "null" | "macvlan" | "ipvlan" | "host" => {}
        other => {
            return settings(format!(
                "--driver {other}: shards' networks are bridge, macvlan, ipvlan, host and null: it has no plugins"
            ));
        }
    }
    if !config_only && (driver == "host" || (driver == "null" && !config_only)) {
        let kind = if driver == "host" { "host" } else { "null" };
        return settings(format!("--driver {kind}: shards has one {kind} network already"));
    }
    let ipam_driver = &ask.ipam_driver;
    if !matches!(ipam_driver.as_str(), "default" | "") {
        return settings(format!(
            "--ipam-driver {ipam_driver}: shards gives a network's addresses itself (default): it has no plugins"
        ));
    }
    if scope == "swarm" {
        return settings("--scope swarm: shards has no swarm mode".into());
    }
    if ask.ingress {
        return settings("--ingress: shards has no swarm mode, whose ingress network it is".into());
    }
    if !ipv4 && !ipv6 {
        return settings("--ipv4=false with --ipv6=false: a network needs addresses of one family".into());
    }
    let Ok(id) = crate::containers::new_id() else {
        return Err((Unmade::Host, "making the network's ID".into()));
    };
    let mut options = from.as_ref().map_or(options, |f| f.options.clone());
    let configs = from.as_ref().map_or(configs, |f| {
        f.pools
            .iter()
            .map(|p| Asked {
                subnet: super::show(p.subnet),
                ip_range: p.ip_range.map(super::show).unwrap_or_default(),
                gateway: p.gateway.to_string(),
                aux: p.aux.iter().map(|(k, v)| (k.clone(), v.to_string())).collect(),
            })
            .collect()
    });
    // The IPv4 pool, allocated (none for a configuration network, which allocates
    // nothing).
    let (v4, v6): (Vec<&Asked>, Vec<&Asked>) = configs
        .iter()
        .partition(|c| netip_prefix(&c.subnet).is_ok_and(|(ip, _)| ip.is_ipv4()));
    let mut pools = Vec::new();
    if ipv4 && !config_only {
        if driver == "bridge" && v4.len() > 1 {
            return settings("a bridge network has one IPv4 subnet in shards, yet".into());
        }
        let asked = v4.first();
        let explicit = asked.and_then(|c| super::parse_prefix(&c.subnet));
        // Explicit pools are checked against the networks' alone; the daemon's own picks
        // also keep off the host's (InferReservedNetworks).
        let mut taken: Vec<Prefix> = networks
            .iter()
            .flat_map(|n| n.pools.iter().map(|p| p.subnet))
            .collect();
        if explicit.is_none_or(|(a, _)| a.is_unspecified()) {
            taken
                .extend(shards_net::bridge::reserved(&shards_net::bridge::host_resolv()).unwrap_or_default());
        }
        let range = asked.and_then(|c| super::parse_prefix(&c.ip_range));
        let gateway = asked.and_then(|c| c.gateway.parse().ok());
        let aux: BTreeMap<String, Ipv4Addr> = asked
            .map(|c| {
                c.aux
                    .iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.parse().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        match super::new_pool(explicit, range, gateway, &aux, &taken) {
            Ok(p) => pools.push(p),
            Err(e) => return Err((Unmade::Pool, e)),
        }
    }
    let mut pools6 = Vec::new();
    if ipv6 && !config_only {
        for c in &v6 {
            let gateway = if c.gateway.is_empty() {
                go_parse_cidr(&c.subnet)
                    .ok()
                    .and_then(|(_, net, _)| match net {
                        std::net::IpAddr::V6(n) => Some(Ipv6Addr::from(u128::from(n) + 1).to_string()),
                        std::net::IpAddr::V4(_) => None,
                    })
                    .unwrap_or_default()
            } else {
                c.gateway.clone()
            };
            pools6.push(Pool6 {
                subnet: c.subnet.clone(),
                ip_range: c.ip_range.clone(),
                gateway,
                aux: c.aux.clone(),
            });
        }
        if pools6.is_empty() {
            // The ULA pool dockerd derives from its host's ID, given out a /64 at a
            // time; its last /64 the host's network's (`host_ipv6`).
            let base = super::ula_base(home);
            let used: Vec<String> = networks
                .iter()
                .flat_map(|n| n.pools6.iter().map(|p| p.subnet.clone()))
                .collect();
            let subnet = (0u128..(1 << 16) - 1)
                .map(|i| format!("{}/64", Ipv6Addr::from(base | (i << 64))))
                .find(|s| !used.contains(s))
                .unwrap_or_default();
            let gateway = subnet
                .split_once('/')
                .and_then(|(a, _)| a.parse::<Ipv6Addr>().ok())
                .map(|a| Ipv6Addr::from(u128::from(a) + 1).to_string())
                .unwrap_or_default();
            pools6.push(Pool6 {
                subnet,
                ip_range: String::new(),
                gateway,
                aux: BTreeMap::new(),
            });
        }
    }
    // The bridge driver's own options, each read as dockerd reads it (moby
    // docker-v29.3.1, drivers/bridge/bridge_linux.go fromLabels, in a Go map's order,
    // here the options' own), then its checks of them (parseNetworkOptions, Validate).
    if driver == "bridge" && !config_only {
        for (label, value) in &options {
            let why = match label.as_str() {
                super::MTU => shards_cmdline::go::atoi(value).err().map(|e| e.to_string()),
                super::MASQUERADE | super::ICC | super::INHIBIT_IPV4 | super::DEFAULT_BRIDGE => {
                    shards_cmdline::go::parse_bool(value).err().map(|e| e.to_string())
                }
                super::GATEWAY_MODE4 | super::GATEWAY_MODE6 => {
                    (!matches!(value.as_str(), "nat" | "nat-unprotected" | "routed" | "isolated"))
                        .then(|| format!("unknown gateway mode {value}"))
                }
                super::HOST_BINDING | super::HOST_IPV4 | super::HOST_IPV6 => value
                    .parse::<std::net::IpAddr>()
                    .is_err()
                    .then(|| "nil ip".to_string()),
                _ => None,
            };
            if let Some(why) = why {
                return settings(format!("failed to parse {label} value: {value} ({why})"));
            }
        }
        let isolated = [super::GATEWAY_MODE4, super::GATEWAY_MODE6]
            .iter()
            .any(|m| options.get(*m).is_some_and(|v| v == "isolated"));
        if isolated && !ask.internal {
            return settings("gateway mode 'isolated' can only be used for an internal network".into());
        }
        if let Some(mtu) = options
            .get(super::MTU)
            .and_then(|v| shards_cmdline::go::atoi(v).ok())
            && mtu < 0
        {
            return settings(format!("invalid MTU number: {mtu}"));
        }
        // A network's bridge name is a Linux bridge's interface name, which shards makes
        // none of: no length of an interface's, no two networks' bridge in common, to
        // refuse.
    }
    // shards' own option (D192): what its members' commands may reach past their
    // microVMs, which only a bridge network's members reach past them at all, and an
    // internal one's never; a config-only network keeps it for those made from it,
    // which take it as theirs.
    let egress = from
        .as_ref()
        .map_or(options.get(super::EGRESS), |f| f.options.get(super::EGRESS));
    if let Some(value) = egress {
        if let Err(why) = super::parse_egress(value) {
            return settings(format!("--opt {}: {why}", super::EGRESS));
        }
        if driver != "bridge" && !config_only {
            return settings(format!(
                "--opt {}: only a bridge network's members reach past their microVMs",
                super::EGRESS
            ));
        }
        if ask.internal {
            return settings(format!(
                "--opt {}: an internal network's members reach nothing past their microVMs",
                super::EGRESS
            ));
        }
    }
    if from.is_some() {
        options.clear();
    }
    let n = Network {
        name: name.clone(),
        id,
        created: crate::containers::now(),
        scope: if scope.is_empty() { "local".into() } else { scope },
        driver: driver.into(),
        ipv4: ipv4 && !config_only,
        ipv6: ipv6 && !config_only,
        ipam_driver: "default".into(),
        ipam_options: ask.ipam_options.clone(),
        pools,
        pools6,
        v6_first: v4.is_empty() && !v6.is_empty(),
        internal: ask.internal,
        attachable: ask.attachable,
        ingress: false,
        config_from: config_from.clone(),
        config_only,
        options: from.map_or(options, |f| f.options),
        labels: ask.labels.clone(),
    };
    Store::new(home).put(&n).map_err(|e| (Unmade::Host, e))?;
    Ok(n)
}

/// Network `n`, one of `networks` (the home's, read under `held`, its networks lock),
/// removed, its leases then its record: refused while networks take their settings from it
/// (a config-only one's), and while a lease of it lives or a process links its runs (D285:
/// every process's run on it holds an address lease, the daemon's as an orchestrator's run
/// hosts'), never waiting for either to let go.
pub fn remove(
    home: &Path,
    held: &leases::Held,
    networks: &[Network],
    n: &Network,
) -> Result<(), (Kept, String)> {
    if n.config_only && networks.iter().any(|o| o.config_from == n.name) {
        return Err((
            Kept::InUse,
            format!(
                "cannot remove network {}: networks take their settings from it",
                n.name
            ),
        ));
    }
    match Leases::of(home, &n.id).and_then(|ledger| ledger.clear(held)) {
        // No process takes a lease of a network whose ID is not one: it has none.
        Ok(()) | Err(leases::Refused::NotAnId) => {}
        Err(leases::Refused::InUse) => {
            return Err((
                Kept::InUse,
                format!(
                    "cannot remove network {}: another process's runs are on it",
                    n.name
                ),
            ));
        }
        Err(e) => return Err((Kept::Host, format!("network {}: {e}", n.name))),
    }
    Store::new(home).remove(&n.id).map_err(|e| (Kept::Host, e))
}

/// Network `name` of `home` removed as [`remove`] removes one, under the home's networks lock,
/// found by its name alone, never by its ID's beginning as `network rm` finds one, so that a
/// name whose network has gone names no other: refused [`Kept::Shards`] for one of shards'
/// own, [`Kept::Unknown`] where no network is named so.
pub fn remove_named(home: &Path, name: &str) -> Result<(), (Kept, String)> {
    if super::PREDEFINED.contains(&name) {
        return Err((
            Kept::Shards,
            format!("{name} is one of shards' own networks, and stays"),
        ));
    }
    let held = leases::hold(home).map_err(|e| (Kept::Host, e.to_string()))?;
    let networks = Store::new(home).list();
    let n = networks
        .iter()
        .find(|n| n.name == name)
        .ok_or_else(|| (Kept::Unknown, format!("no network is named {name}")))?;
    remove(home, &held, &networks, n)
}

/// `s` as ADDRESS/BITS (Go's net.ParseCIDR): its address and its network, or why not.
fn go_parse_cidr(s: &str) -> Result<(std::net::IpAddr, std::net::IpAddr, u8), String> {
    let bad = || format!("{s} is no subnet: give ADDRESS/BITS, as 10.10.0.0/16");
    let (addr, bits) = s.split_once('/').ok_or_else(bad)?;
    let ip: std::net::IpAddr = addr.parse().map_err(|_| bad())?;
    let bits: u8 = bits.parse().map_err(|_| bad())?;
    let net = match ip {
        std::net::IpAddr::V4(v4) if bits <= 32 => {
            let m = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            std::net::IpAddr::V4(Ipv4Addr::from(u32::from(v4) & m))
        }
        std::net::IpAddr::V6(v6) if bits <= 128 => {
            let m = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            std::net::IpAddr::V6(Ipv6Addr::from(u128::from(v6) & m))
        }
        _ => return Err(bad()),
    };
    Ok((ip, net, bits))
}

/// Whether `ip` is in the network `net`/`bits`.
fn net_contains(net: std::net::IpAddr, bits: u8, ip: std::net::IpAddr) -> bool {
    match (net, ip) {
        (std::net::IpAddr::V4(n), std::net::IpAddr::V4(i)) => {
            let m = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(n) & m == u32::from(i) & m
        }
        (std::net::IpAddr::V6(n), std::net::IpAddr::V6(i)) => {
            let m = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(n) & m == u128::from(i) & m
        }
        _ => false,
    }
}

/// subnetMatches(subnet, data): whether `data`'s address is in `subnet`.
fn subnet_matches(subnet: &str, data: &str) -> Result<bool, String> {
    let (_, net, bits) = go_parse_cidr(subnet).map_err(|e| format!("--subnet {e}"))?;
    Ok(go_parse_cidr(data).is_ok_and(|(ip, _, _)| net_contains(net, bits, ip)))
}

/// `s` as ADDRESS/BITS, the address and the prefix's length (Go's netip.ParsePrefix: no
/// zero before a length), or why not.
fn netip_prefix(s: &str) -> Result<(std::net::IpAddr, u8), String> {
    let Some((addr, bits)) = s.split_once('/') else {
        return Err(format!("--subnet {s}: give ADDRESS/BITS, as 10.10.0.0/16"));
    };
    let ip: std::net::IpAddr = addr
        .parse()
        .map_err(|_| format!("--subnet {s}: {addr} is no IP address"))?;
    let ok = !bits.is_empty()
        && bits.bytes().all(|b| b.is_ascii_digit())
        && !(bits.len() > 1 && bits.starts_with('0'));
    let n: u32 = if ok {
        bits.parse().unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    if !ok {
        return Err(format!("--subnet {s}: {bits} is no prefix length"));
    }
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if n > max {
        return Err(format!(
            "--subnet {s}: an IPv{} prefix is at most {max} bits",
            if max == 32 { 4 } else { 6 }
        ));
    }
    Ok((ip, u8::try_from(n).unwrap_or(0)))
}

/// One IPAM config as the CLI consolidates it: the subnet as given, its range and gateway
/// as given, its auxiliary addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Asked {
    subnet: String,
    ip_range: String,
    gateway: String,
    aux: BTreeMap<String, String>,
}

/// The subnets, ranges, gateways and auxiliary addresses a network is made with, each
/// range, gateway and address matched to its subnet, checked in the order createIPAMConfig
/// checks them (cli/command/network/create.go, consolidateIpam).
fn consolidate(
    subnets: &[String],
    ranges: &[String],
    gateways: &[String],
    aux: &BTreeMap<String, String>,
) -> Result<Vec<Asked>, String> {
    if subnets.len() < ranges.len() || subnets.len() < gateways.len() {
        return Err("--ip-range and --gateway each belong to a --subnet: give its subnet".into());
    }
    let mut configs: Vec<Asked> = Vec::new();
    for s in subnets {
        for k in &configs {
            if subnet_matches(s, &k.subnet)? || subnet_matches(&k.subnet, s)? {
                return Err(format!("--subnet {s} overlaps --subnet {}", k.subnet));
            }
        }
        netip_prefix(s)?;
        configs.push(Asked {
            subnet: s.clone(),
            ..Asked::default()
        });
    }
    for r in ranges {
        let mut matched = false;
        for c in &mut configs {
            if subnet_matches(&c.subnet, r)? {
                if !c.ip_range.is_empty() {
                    return Err(format!(
                        "--ip-range {r} and --ip-range {}: subnet {} takes one range",
                        c.ip_range, c.subnet
                    ));
                }
                c.ip_range = r.clone();
                matched = true;
            }
        }
        if !matched {
            return Err(format!("--ip-range {r}: in none of the subnets given"));
        }
    }
    for g in gateways {
        let mut matched = false;
        for c in &mut configs {
            let (_, net, bits) = go_parse_cidr(&c.subnet).map_err(|e| format!("--subnet {e}"))?;
            let ip: Option<std::net::IpAddr> = g.parse().ok();
            if ip.is_some_and(|ip| net_contains(net, bits, ip)) {
                if !c.gateway.is_empty() {
                    return Err(format!(
                        "--gateway {g} and --gateway {}: subnet {} takes one gateway",
                        c.gateway, c.subnet
                    ));
                }
                c.gateway = g.clone();
                matched = true;
            }
        }
        if !matched {
            return Err(format!("--gateway {g}: in none of the subnets given"));
        }
    }
    for (name, value) in aux {
        if value.is_empty() {
            continue;
        }
        let ip: std::net::IpAddr = value
            .parse()
            .map_err(|_| format!("--aux-address {name}={value}: {value} is no IP address"))?;
        let mut matched = false;
        for c in &mut configs {
            let (_, net, bits) = go_parse_cidr(&c.subnet).map_err(|e| format!("--subnet {e}"))?;
            if net_contains(net, bits, ip) {
                c.aux.insert(name.clone(), value.clone());
                matched = true;
            }
        }
        if !matched {
            return Err(format!(
                "--aux-address {name}={value}: in none of the subnets given"
            ));
        }
    }
    Ok(configs)
}

/// validateIpamConfig's errors for `c` (one IPv4 or IPv6 config the daemon keeps).
fn ipam_errors(c: &Asked) -> Vec<String> {
    let mut errs = Vec::new();
    let Ok((ip, bits)) = netip_prefix(&c.subnet) else {
        return errs;
    };
    let Ok((_, net, _)) = go_parse_cidr(&c.subnet) else {
        return errs;
    };
    if ip != net {
        errs.push(format!(
            "--subnet {}: its network's address is {net}/{bits}",
            c.subnet
        ));
    }
    let family = |a: std::net::IpAddr| if a.is_ipv4() { 4 } else { 6 };
    if !c.ip_range.is_empty()
        && let Ok((rip, rnet, rbits)) = go_parse_cidr(&c.ip_range)
    {
        if family(rip) != family(net) {
            errs.push(format!(
                "--ip-range {}: its subnet is IPv{}",
                c.ip_range,
                family(net)
            ));
        } else if rbits < bits {
            errs.push(format!(
                "--ip-range {}: larger than its subnet {}",
                c.ip_range, c.subnet
            ));
        } else if !net_contains(net, bits, rnet) {
            errs.push(format!(
                "--ip-range {}: outside its subnet {}",
                c.ip_range, c.subnet
            ));
        }
    }
    if let Ok(gw) = c.gateway.parse::<std::net::IpAddr>() {
        if family(gw) != family(net) {
            errs.push(format!(
                "--gateway {}: its subnet is IPv{}",
                c.gateway,
                family(net)
            ));
        } else if !net_contains(net, bits, gw) {
            errs.push(format!(
                "--gateway {}: outside its subnet {}",
                c.gateway, c.subnet
            ));
        }
    }
    for (name, value) in &c.aux {
        if let Ok(a) = value.parse::<std::net::IpAddr>() {
            if family(a) != family(net) {
                errs.push(format!("--aux-address {name}: its subnet is IPv{}", family(net)));
            } else if !net_contains(net, bits, a) {
                errs.push(format!("--aux-address {name}: outside its subnet {}", c.subnet));
            }
        }
    }
    errs
}

/// What the default network does, said as its options (D31): its flows leave from its
/// network process's host sockets, under the host's address, as masquerading makes them; a
/// port published with no address is bound at every one (publish::bind); no member
/// reaches another, as each has the same address, so that one template serves them all
/// (shards_net::bridge); and its interface's MTU is its device's.
fn bridge_options() -> BTreeMap<String, String> {
    [
        (super::DEFAULT_BRIDGE, "true".to_string()),
        (super::ICC, "false".to_string()),
        (super::MASQUERADE, "true".to_string()),
        (super::HOST_BINDING, "0.0.0.0".to_string()),
        (super::MTU, shards_vmm::devices::virtio::net::MTU.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// A network's subnets, ranges, gateways and auxiliary addresses, each checked and
    /// refused in shards' words, in the order docker/cli 29.3.1 checks them.
    #[test]
    fn ipam_configs_are_consolidated() {
        let none = BTreeMap::new();
        assert_eq!(
            consolidate(&[], &[], &s(&["10.0.0.1"]), &none).unwrap_err(),
            "--ip-range and --gateway each belong to a --subnet: give its subnet"
        );
        assert_eq!(
            consolidate(&s(&["10.10.0.0/16", "10.10.1.0/24"]), &[], &[], &none).unwrap_err(),
            "--subnet 10.10.1.0/24 overlaps --subnet 10.10.0.0/16"
        );
        assert_eq!(
            consolidate(&s(&["bogus"]), &[], &[], &none).unwrap_err(),
            "--subnet bogus: give ADDRESS/BITS, as 10.10.0.0/16"
        );
        assert_eq!(
            consolidate(&s(&["10.10.0.0/33"]), &[], &[], &none).unwrap_err(),
            "--subnet 10.10.0.0/33: an IPv4 prefix is at most 32 bits"
        );
        assert_eq!(
            consolidate(
                &s(&["10.10.0.0/16", "10.20.0.0/16"]),
                &s(&["10.10.1.0/24", "10.10.0.0/24"]),
                &[],
                &none
            )
            .unwrap_err(),
            "--ip-range 10.10.0.0/24 and --ip-range 10.10.1.0/24: subnet 10.10.0.0/16 takes one range"
        );
        assert_eq!(
            consolidate(&s(&["10.10.0.0/16"]), &[], &s(&["10.20.0.1"]), &none).unwrap_err(),
            "--gateway 10.20.0.1: in none of the subnets given"
        );
        let aux = BTreeMap::from([("a".to_string(), "bogus".to_string())]);
        assert_eq!(
            consolidate(&s(&["10.10.0.0/16"]), &[], &[], &aux).unwrap_err(),
            "--aux-address a=bogus: bogus is no IP address"
        );
        let c = consolidate(&s(&["10.10.0.5/16"]), &[], &[], &none).unwrap();
        assert_eq!(
            ipam_errors(&c[0]),
            ["--subnet 10.10.0.5/16: its network's address is 10.10.0.0/16"]
        );
        let c = consolidate(&s(&["10.10.0.0/24"]), &s(&["10.10.0.0/16"]), &[], &none).unwrap();
        assert_eq!(
            ipam_errors(&c[0]),
            ["--ip-range 10.10.0.0/16: larger than its subnet 10.10.0.0/24"]
        );
    }

    /// D285 slice 6: a network made by its name alone, as an orchestrator makes the one kiln's
    /// `Change::Connect` names, is a bridge network of IPv4 on the lowest of dockerd's pools
    /// the default network's subnet leaves free, kept in the home with shards' own beside it;
    /// made again, refused `Exists`; one of shards' own names, or none, refused `Name`. Removed
    /// by its name, it is gone with its leases; removed again, `Unknown`; one of shards' own,
    /// `Shards`. While a lease of it lives, as a run on it holds one, its removal is refused
    /// `InUse` and it stays; once that lease is let go, it is removed.
    #[test]
    fn a_network_is_made_by_its_name_and_removed_once_no_run_holds_it() {
        let dir = shards_testdir::TempDir::new("net-made").unwrap();
        let home = dir.path();
        let bridge = Bridge::elect(&[]);
        let made = create(home, bridge, &Create::named("team")).unwrap();
        assert_eq!(
            (made.driver.as_str(), made.ipv4, made.ipv6),
            ("bridge", true, false)
        );
        let subnet = made.pools.first().unwrap().subnet;
        assert!(
            bridge.is_none_or(|b| !shards_net::bridge::overlap(b.subnet(), subnet)),
            "{subnet:?} meets the default network's"
        );
        let names: Vec<String> = Store::new(home).list().into_iter().map(|n| n.name).collect();
        for name in ["team", "bridge", "host", "none"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        assert_eq!(
            create(home, bridge, &Create::named("team"))
                .map(|n| n.id)
                .unwrap_err()
                .0,
            Unmade::Exists
        );
        for name in ["bridge", "default", " "] {
            assert_eq!(
                create(home, bridge, &Create::named(name))
                    .map(|n| n.id)
                    .unwrap_err()
                    .0,
                Unmade::Name,
                "{name:?}"
            );
        }
        let pool = made.pools.first().unwrap().clone();
        let holder = "a".repeat(2 * crate::containers::ID_BYTES);
        let lease = {
            let held = leases::hold(home).unwrap();
            Leases::of(home, &made.id)
                .unwrap()
                .take(&held, &pool, None, &holder)
                .unwrap()
        };
        assert_eq!(remove_named(home, "team").unwrap_err().0, Kept::InUse);
        assert!(Store::new(home).list().iter().any(|n| n.name == "team"), "kept");
        drop(lease);
        assert_eq!(remove_named(home, "team"), Ok(()));
        assert!(!Store::new(home).list().iter().any(|n| n.name == "team"));
        assert!(!home.join("networks").join(format!("{}.leases", made.id)).exists());
        assert_eq!(remove_named(home, "team").unwrap_err().0, Kept::Unknown);
        assert_eq!(remove_named(home, "bridge").unwrap_err().0, Kept::Shards);
    }

    /// A name is found by itself alone: one that is no network's but begins another's ID is
    /// refused `Unknown`, that network kept.
    #[test]
    fn a_removal_by_name_never_takes_a_network_by_its_ids_beginning() {
        let dir = shards_testdir::TempDir::new("net-made").unwrap();
        let home = dir.path();
        let made = create(home, Bridge::elect(&[]), &Create::named("team")).unwrap();
        let beginning = made.id.get(..4).unwrap();
        assert_eq!(remove_named(home, beginning).unwrap_err().0, Kept::Unknown);
        assert!(Store::new(home).list().iter().any(|n| n.id == made.id));
    }
}
