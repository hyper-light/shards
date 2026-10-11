//! `shards network` (D46): user-defined networks as dockerd keeps them (moby docker-v29.3.1
//! at f78c987a: daemon/network.go, libnetwork, defaultipam), dockerd's predefined `bridge`,
//! `host` and `none` beside them, and what docker/cli checks before it asks
//! (cli/command/network/create.go createIPAMConfig). Their members are the running
//! microVMs on them, which the daemon holds as it starts and ends their runs.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use shards_template::{Kind, Struct, Value};

use super::commands::{Asker, Reply};
use super::filters::Filters;
use super::{Daemon, lock};
use crate::containers::Disk;
use crate::networks::{self as nets, Found, Network, Pool, Pool6, Store};

/// The filters `network ls` takes (daemon/network/filter.go).
const LIST_FILTERS: [&str; 7] = ["dangling", "driver", "id", "label", "name", "scope", "type"];
/// The filters `network prune` takes.
const PRUNE_FILTERS: [&str; 3] = ["label", "label!", "until"];

/// A running microVM's endpoint on a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Member {
    pub container: String,
    pub name: String,
    pub endpoint: String,
    pub mac: String,
    pub ip: Ipv4Addr,
    /// Its IPv6 address, on a network with IPv6 (D99).
    pub ip6: Option<Ipv6Addr>,
    /// What its peers' resolver answers for it (DNSNames): its name, aliases, short ID
    /// and host name, the first its PTR's.
    pub dns_names: Vec<String>,
    /// Its link-local addresses while it runs (PM M175): those given it, and on a network
    /// with IPv6 the one its kernel makes of its MAC.
    pub link_local: Vec<std::net::IpAddr>,
}

/// Sends a network process `which` and waits for it to say it took it, as published
/// ports are given (`give_ports`).
pub(super) fn ask_net(
    net: &std::os::unix::net::UnixStream,
    which: u8,
    payload: &[u8],
    fds: &[std::os::fd::BorrowedFd<'_>],
) -> Result<(), String> {
    net.set_read_timeout(Some(super::PUBLISH_PATIENCE))
        .map_err(|e| e.to_string())?;
    shards_ipc::send(net, which, payload, fds).map_err(|e| format!("the network process: {e}"))?;
    match shards_ipc::recv(net) {
        Ok(Some(m)) if m.kind == which => Ok(()),
        Ok(_) => Err("the network process did not take it".into()),
        Err(e) => Err(format!("the network process: {e}")),
    }
}

/// The time `ns` as Go's RFC3339Nano writes it in UTC.
fn rfc3339_nano(ns: u128) -> String {
    let secs = i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX);
    let frac = ns % 1_000_000_000;
    let base = shards_cmdline::format::rfc3339_at(secs, 0);
    let base = base.strip_suffix('Z').unwrap_or(&base).to_string();
    if frac == 0 {
        format!("{base}Z")
    } else {
        let digits = format!("{frac:09}");
        format!("{base}.{}Z", digits.trim_end_matches('0'))
    }
}

/// A random 64-hex ID, as dockerd's stringid makes a network's.
fn new_id() -> Result<String, String> {
    crate::containers::new_id().map_err(|e| e.to_string())
}

/// A `k=v` list as a map, text without `=` a key of no value (opts.ConvertKVStringsToMap,
/// MapOpts).
fn kv_map(given: &[String]) -> BTreeMap<String, String> {
    given
        .iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.clone(), String::new()),
        })
        .collect()
}

/// Go's net.ParseCIDR of `s`: its address and its network, or the error's words.
fn go_parse_cidr(s: &str) -> Result<(std::net::IpAddr, std::net::IpAddr, u8), String> {
    let bad = || format!("invalid CIDR address: {s}");
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
    let (_, net, bits) = go_parse_cidr(subnet).map_err(|e| format!("invalid subnet: {e}"))?;
    Ok(go_parse_cidr(data).is_ok_and(|(ip, _, _)| net_contains(net, bits, ip)))
}

/// netip.ParsePrefix's words for `s`, as the CLI prints them.
fn netip_prefix(s: &str) -> Result<(std::net::IpAddr, u8), String> {
    let quoted = shards_cmdline::go::quote(s);
    let Some((addr, bits)) = s.split_once('/') else {
        return Err(format!("netip.ParsePrefix({quoted}): no '/'"));
    };
    let ip: std::net::IpAddr = addr.parse().map_err(|_| {
        format!(
            "netip.ParsePrefix({quoted}): ParseAddr({}): unable to parse IP",
            shards_cmdline::go::quote(addr)
        )
    })?;
    let ok = !bits.is_empty()
        && bits.bytes().all(|b| b.is_ascii_digit())
        && !(bits.len() > 1 && bits.starts_with('0'));
    let n: u32 = if ok {
        bits.parse().unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    if !ok {
        return Err(format!(
            "netip.ParsePrefix({quoted}): bad bits after slash: {}",
            shards_cmdline::go::quote(bits)
        ));
    }
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if n > max {
        return Err(format!("netip.ParsePrefix({quoted}): prefix length out of range"));
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

/// createIPAMConfig (cli/command/network/create.go, consolidateIpam), in its order and
/// words, which the CLI prints bare.
fn consolidate(
    subnets: &[String],
    ranges: &[String],
    gateways: &[String],
    aux: &BTreeMap<String, String>,
) -> Result<Vec<Asked>, String> {
    if subnets.len() < ranges.len() || subnets.len() < gateways.len() {
        return Err("every ip-range or gateway must have a corresponding subnet".into());
    }
    let mut configs: Vec<Asked> = Vec::new();
    for s in subnets {
        for k in &configs {
            if subnet_matches(s, &k.subnet)? || subnet_matches(&k.subnet, s)? {
                return Err("multiple overlapping subnet configuration is not supported".into());
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
                        "cannot configure multiple ranges ({r}, {}) on the same subnet ({})",
                        c.ip_range, c.subnet
                    ));
                }
                c.ip_range = r.clone();
                matched = true;
            }
        }
        if !matched {
            return Err(format!("no matching subnet for range {r}"));
        }
    }
    for g in gateways {
        let mut matched = false;
        for c in &mut configs {
            let (_, net, bits) = go_parse_cidr(&c.subnet).map_err(|e| format!("invalid subnet: {e}"))?;
            let ip: Option<std::net::IpAddr> = g.parse().ok();
            if ip.is_some_and(|ip| net_contains(net, bits, ip)) {
                if !c.gateway.is_empty() {
                    return Err(format!(
                        "cannot configure multiple gateways ({g}, {}) for the same subnet ({})",
                        c.gateway, c.subnet
                    ));
                }
                c.gateway = g.clone();
                matched = true;
            }
        }
        if !matched {
            return Err(format!("no matching subnet for gateway {g}"));
        }
    }
    for (name, value) in aux {
        if value.is_empty() {
            continue;
        }
        let ip: std::net::IpAddr = value.parse().map_err(|_| {
            format!(
                "ParseAddr({}): unable to parse IP",
                shards_cmdline::go::quote(value)
            )
        })?;
        let mut matched = false;
        for c in &mut configs {
            let (_, net, bits) = go_parse_cidr(&c.subnet).map_err(|e| format!("invalid subnet: {e}"))?;
            if net_contains(net, bits, ip) {
                c.aux.insert(name.clone(), value.clone());
                matched = true;
            }
        }
        if !matched {
            return Err(format!("no matching subnet for aux-address {value}"));
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
        errs.push(format!("invalid subnet {}: it should be {net}/{bits}", c.subnet));
    }
    let family = |a: std::net::IpAddr| if a.is_ipv4() { 4 } else { 6 };
    if !c.ip_range.is_empty()
        && let Ok((rip, rnet, rbits)) = go_parse_cidr(&c.ip_range)
    {
        if family(rip) != family(net) {
            errs.push(format!(
                "invalid ip-range {}: parent subnet is an IPv{} block",
                c.ip_range,
                family(net)
            ));
        } else if rbits < bits {
            errs.push(format!(
                "invalid ip-range {}: CIDR block is bigger than its parent subnet {}",
                c.ip_range, c.subnet
            ));
        } else if !net_contains(net, bits, rnet) {
            errs.push(format!(
                "invalid ip-range {}: parent subnet {} doesn't contain ip-range",
                c.ip_range, c.subnet
            ));
        }
    }
    if let Ok(gw) = c.gateway.parse::<std::net::IpAddr>() {
        if family(gw) != family(net) {
            errs.push(format!(
                "invalid gateway {}: parent subnet is an IPv{} block",
                c.gateway,
                family(net)
            ));
        } else if !net_contains(net, bits, gw) {
            errs.push(format!(
                "invalid gateway {}: parent subnet {} doesn't contain this address",
                c.gateway, c.subnet
            ));
        }
    }
    for (name, value) in &c.aux {
        if let Ok(a) = value.parse::<std::net::IpAddr>() {
            if family(a) != family(net) {
                errs.push(format!(
                    "invalid auxiliary address {name}: parent subnet is an IPv{} block",
                    family(net)
                ));
            } else if !net_contains(net, bits, a) {
                errs.push(format!(
                    "invalid auxiliary address {name}: parent subnet {} doesn't contain this address",
                    c.subnet
                ));
            }
        }
    }
    errs
}

/// dockerd's multi-error: one as itself, several as a list (multierror.Join).
fn joined(head: &str, errs: &[String]) -> String {
    match errs {
        [one] => format!("{head}\n{one}"),
        many => {
            let lines: Vec<String> = many.iter().map(|e| format!("* {e}")).collect();
            format!("{head}\n{}", lines.join("\n"))
        }
    }
}

impl<D: Disk> Daemon<D> {
    /// The home's engine ID, made once: what dockerd derives its IPv6 ULA pool from.
    fn engine_id(&self) -> String {
        let path = self.home.join("networks").join("engine-id");
        if let Ok(id) = std::fs::read_to_string(&path) {
            return id.trim().to_string();
        }
        let id = new_id().unwrap_or_default();
        let _ = crate::networks::make_dir(&self.home.join("networks"));
        let _ = std::fs::write(&path, &id);
        id
    }

    /// The networks: dockerd's predefined ones, made here the first time they are
    /// asked for, the bridge's on the subnet this daemon elected, then the user's.
    pub(super) fn all_networks(&self) -> Vec<Network> {
        let store = Store::new(&self.home);
        let mut all = store.list();
        for (name, driver) in [("bridge", "bridge"), ("host", "host"), ("none", "null")] {
            let pools: Vec<Pool> = if name == "bridge" {
                self.bridge
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
            match all.iter_mut().find(|n| n.name == name) {
                Some(n) if n.pools != pools => {
                    n.pools = pools;
                    let _ = store.put(n);
                }
                Some(_) => {}
                None => {
                    let Ok(id) = new_id() else { continue };
                    let options = if name == "bridge" {
                        [
                            ("com.docker.network.bridge.default_bridge", "true"),
                            ("com.docker.network.bridge.enable_icc", "true"),
                            ("com.docker.network.bridge.enable_ip_masquerade", "true"),
                            ("com.docker.network.bridge.host_binding_ipv4", "0.0.0.0"),
                            ("com.docker.network.bridge.name", "docker0"),
                            ("com.docker.network.driver.mtu", "1500"),
                        ]
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect()
                    } else {
                        BTreeMap::new()
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

    /// MicroVM `given`'s kept request, and its ID, for connect and disconnect; dockerd's
    /// words where there is none.
    fn request_of(&self, given: &str) -> Result<(String, shards_ipc::Run), String> {
        let id = self.resolve(given).map_err(|e| {
            e.strip_prefix("Error response from daemon: ")
                .unwrap_or(&e)
                .to_string()
        })?;
        let dir = lock(&self.containers).dir(&id);
        let run = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .ok_or_else(|| format!("container {id}: its request is not kept"))?;
        Ok((id, run))
    }

    fn keep_request(&self, id: &str, run: &shards_ipc::Run) -> Result<(), String> {
        let dir = lock(&self.containers).dir(id);
        std::fs::write(dir.join(super::REQUEST), run.encode()).map_err(|e| e.to_string())
    }

    /// `shards network connect NETWORK CONTAINER` (NetworkConnect, ConnectToNetwork): a
    /// created or stopped microVM's endpoint on the network, kept for its next start;
    /// one it has already, kept as it was; one on a network not there, kept unchecked, as
    /// dockerd keeps it, for its start to refuse. A running microVM has the one
    /// network device it started with: one more, live, shards does not give yet.
    pub(super) fn network_connect(&self, parsed: &shards_cmdline::flags::Parsed, reply: &Reply<'_>) -> u8 {
        let (Some(term), Some(given)) = (parsed.args.first(), parsed.args.get(1)) else {
            return 1;
        };
        let daemon = |e: &str| {
            reply.err(&format!("Error response from daemon: {e}"));
            1
        };
        let (id, mut run) = match self.request_of(given) {
            Ok(r) => r,
            Err(e) => return daemon(&e),
        };
        let networks = self.all_networks();
        let found = match nets::find(&networks, term) {
            Found::One(n) => Some(n.clone()),
            Found::Ambiguous(count, by_name) => {
                return daemon(&format!(
                    "network {term} is ambiguous ({count} matches found {})",
                    if by_name { "on name" } else { "based on ID prefix" }
                ));
            }
            Found::None => None,
        };
        let name = found.as_ref().map_or(term.as_str(), |n| n.name.as_str());
        let mode = match run.network.as_str() {
            "" | "default" => "bridge",
            m => m,
        };
        let connected =
            |run: &shards_ipc::Run| mode == name || run.endpoints.iter().any(|e| e.network == name);
        if self.running(&id) {
            let Some(n) = &found else {
                return daemon(&format!("network {term} not found"));
            };
            if connected(&run) {
                let cname = lock(&self.containers)
                    .get(&id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                return daemon(&format!(
                    "endpoint with name {cname} already exists in network {}",
                    n.name
                ));
            }
            return daemon(
                "connecting a running microVM to another network is not supported by shards yet: it has one network device",
            );
        }
        if name == "host" {
            return daemon(
                "cannot connect container to host network - container must be created in host network mode",
            );
        }
        if (name == "none") != (mode == "none") {
            return daemon(
                "container cannot be connected to multiple networks with one of the networks in private (none) mode",
            );
        }
        if found.is_some() && connected(&run) {
            return 0;
        }
        let ip = parsed.string("ip").to_string();
        let ip6 = parsed.string("ip6").to_string();
        let endpoint = shards_ipc::Endpoint {
            network: name.to_string(),
            aliases: parsed.many("alias").to_vec(),
            ipv4: if ip == "<nil>" { String::new() } else { ip },
            ipv6: if ip6 == "<nil>" { String::new() } else { ip6 },
            link_local: parsed.many("link-local-ip").to_vec(),
            mac: String::new(),
            driver_opts: parsed
                .many("driver-opt")
                .iter()
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    (k.trim().to_string(), v.trim().to_string())
                })
                .collect(),
            gw_priority: parsed.int("gw-priority"),
        };
        if endpoint.driver_opts.iter().any(|(k, _)| k.is_empty())
            || parsed.many("driver-opt").iter().any(|kv| !kv.contains('='))
        {
            reply.err("invalid key/value pair format in driver options");
            return 1;
        }
        if let Some(n) = &found {
            let mut errs = Vec::new();
            if n.predefined() {
                if !endpoint.ipv4.is_empty() || !endpoint.ipv6.is_empty() {
                    errs.push(
                        "user-specified IP address is supported on user-defined networks only".to_string(),
                    );
                }
                if !endpoint.aliases.is_empty() {
                    errs.push(
                        "network-scoped aliases are only supported for user-defined networks".to_string(),
                    );
                }
            }
            if let Ok(ip) = endpoint.ipv4.parse::<Ipv4Addr>()
                && !n.pools.iter().any(|p| nets::contains(p.subnet, ip))
            {
                errs.push(format!("no configured subnet contains IP address {ip}"));
            }
            if !errs.is_empty() {
                return daemon(&joined("invalid endpoint settings:", &errs));
            }
        }
        run.endpoints.push(endpoint);
        match self.keep_request(&id, &run) {
            Ok(()) => 0,
            Err(e) => daemon(&e),
        }
    }

    /// `shards network disconnect NETWORK CONTAINER` (NetworkDisconnect): a created or
    /// stopped microVM's endpoint on the network gone, as dockerd removes it; a running
    /// one's, shards does not take away live yet.
    pub(super) fn network_disconnect(&self, parsed: &shards_cmdline::flags::Parsed, reply: &Reply<'_>) -> u8 {
        let (Some(term), Some(given)) = (parsed.args.first(), parsed.args.get(1)) else {
            return 1;
        };
        let daemon = |e: &str| {
            reply.err(&format!("Error response from daemon: {e}"));
            1
        };
        let networks = self.all_networks();
        let found = match nets::find(&networks, term) {
            Found::One(n) => Some(n.clone()),
            Found::Ambiguous(count, by_name) => {
                return daemon(&format!(
                    "network {term} is ambiguous ({count} matches found {})",
                    if by_name { "on name" } else { "based on ID prefix" }
                ));
            }
            Found::None => None,
        };
        let (id, mut run) = match self.request_of(given) {
            Ok(r) => r,
            Err(_) if parsed.bool("force") => {
                return match found {
                    Some(_) => daemon(&format!("endpoint {given} not found")),
                    None => daemon(&format!("network {term} not found")),
                };
            }
            Err(e) => return daemon(&e),
        };
        let name = found.as_ref().map_or(term.as_str(), |n| n.name.as_str());
        let mode = match run.network.as_str() {
            "" | "default" => "bridge",
            m => m,
        };
        if self.running(&id) {
            if found.is_none() {
                return daemon(&format!("network {term} not found"));
            }
            if mode == "host" && name == "host" {
                return daemon(
                    "cannot disconnect container from host network - container was created in host network mode",
                );
            }
            if self.membership(&id).is_none_or(|(n, _)| n.name != name) && mode != name {
                return daemon(&format!("container {id} is not connected to network {name}"));
            }
            return daemon("disconnecting a running microVM from its network is not supported by shards yet");
        }
        let had = run.endpoints.len();
        run.endpoints.retain(|e| e.network != name);
        let primary = mode == name;
        if run.endpoints.len() == had && !primary {
            return daemon(&format!("container {id} is not connected to the network {name}"));
        }
        if primary {
            run.network = "none".into();
        }
        match self.keep_request(&id, &run) {
            Ok(()) => 0,
            Err(e) => daemon(&e),
        }
    }

    /// The one user network `run`, on the default bridge, was connected to (`network
    /// connect`), which it is on alone: a microVM has one network device, and under
    /// default deny the bridge gives it no more than the network does. None for a run on
    /// a network it names, or on none, or connected to more than one.
    pub(super) fn connected_network(&self, run: &shards_ipc::Run) -> Option<String> {
        if !matches!(run.network.as_str(), "" | "default" | "bridge") {
            return None;
        }
        let user: Vec<&String> = run
            .endpoints
            .iter()
            .filter(|e| shards_cmdline::network::is_user_defined(&e.network))
            .map(|e| &e.network)
            .collect();
        match user.as_slice() {
            [only] if self.user_network(only).is_some() => Some((*only).clone()),
            _ => None,
        }
    }

    /// The user network `term` names (FindNetwork): none for a predefined one, one not
    /// there, or a name that is ambiguous.
    pub(super) fn user_network(&self, term: &str) -> Option<Network> {
        let networks = self.all_networks();
        match nets::find(&networks, term) {
            Found::One(n) if !n.predefined() && !n.config_only => Some(n.clone()),
            _ => None,
        }
    }

    /// Network `id`'s members: those whose runs are going, as dockerd's endpoints last as
    /// long as their sandboxes. One whose run has gone, however its start ended, is gone.
    pub(super) fn members_of(&self, id: &str) -> Vec<Member> {
        let runs = lock(&self.runs);
        lock(&self.members)
            .get(id)
            .map(|m| {
                m.iter()
                    .filter(|m| runs.contains_key(&m.container))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// MicroVM `id`'s network and endpoint, while it is a member of one.
    pub(super) fn membership(&self, id: &str) -> Option<(Network, Member)> {
        let networks = self.all_networks();
        lock(&self.members).iter().find_map(|(net, members)| {
            let m = members.iter().find(|m| m.container == id)?;
            Some((networks.iter().find(|n| &n.id == net)?.clone(), m.clone()))
        })
    }

    /// MicroVM `id`'s name, as its endpoint's on its network: DNSNames' first.
    pub(super) fn name_member(&self, id: &str, name: &str) {
        for list in lock(&self.members).values_mut() {
            if let Some(m) = list.iter_mut().find(|m| m.container == id) {
                m.name = name.to_string();
                m.dns_names.retain(|n| n != name);
                m.dns_names.insert(0, name.to_string());
            }
        }
    }

    /// MicroVM `id`'s endpoint on user network `term` (its `--ip`, its aliases), as
    /// libnetwork makes a container's as it starts: its address the asked one, or the
    /// lowest free in the network's range; a member until its run ends. dockerd's words
    /// where the start fails.
    pub(super) fn join_network(
        &self,
        id: &str,
        term: &str,
        asked_ip: &str,
        asked_ip6: &str,
        aliases: &[String],
        hostname: &str,
    ) -> Result<(Network, Member), String> {
        let fail = |e: &str| format!("failed to set up container networking: {e}");
        let _held = nets::lock();
        let network = self
            .user_network(term)
            .ok_or_else(|| fail(&format!("network {term} not found")))?;
        let pool = network
            .pools
            .first()
            .cloned()
            .ok_or_else(|| fail(&format!("network {} has no IPv4 pool", network.name)))?;
        let others: Vec<Ipv4Addr> = self.members_of(&network.id).iter().map(|m| m.ip).collect();
        let ip = if asked_ip.is_empty() {
            nets::allocate(&pool, &others).ok_or_else(|| {
                fail(&format!(
                    "no available IPv4 addresses on this network's address pools: {} ({})",
                    network.name, network.id
                ))
            })?
        } else {
            let ip: Ipv4Addr = asked_ip
                .trim_start_matches("::ffff:")
                .parse()
                .map_err(|_| fail("Address already in use"))?;
            if nets::in_use(&pool, &others).contains(&ip) {
                return Err(fail("Address already in use"));
            }
            ip
        };
        // Its IPv6 address, as its IPv4 one: the asked one, or the lowest free.
        let ip6 = match (network.ipv6, network.pools6.first()) {
            (true, Some(pool6)) => {
                let others6: Vec<Ipv6Addr> = self
                    .members_of(&network.id)
                    .iter()
                    .filter_map(|m| m.ip6)
                    .collect();
                Some(if asked_ip6.is_empty() {
                    nets::allocate6(pool6, &others6).ok_or_else(|| {
                        fail(&format!(
                            "no available IPv6 addresses on this network's address pools: {} ({})",
                            network.name, network.id
                        ))
                    })?
                } else {
                    let ip6: Ipv6Addr = asked_ip6.parse().map_err(|_| fail("Address already in use"))?;
                    if nets::in_use6(pool6, &others6).contains(&ip6) {
                        return Err(fail("Address already in use"));
                    }
                    ip6
                })
            }
            _ => None,
        };
        let short = id.get(..12).unwrap_or(id).to_string();
        let mut dns_names = Vec::new();
        for n in aliases.iter().chain([&short, &hostname.to_string()]) {
            if !n.is_empty() && !dns_names.contains(n) {
                dns_names.push(n.clone());
            }
        }
        let member = Member {
            container: id.to_string(),
            name: String::new(),
            endpoint: new_id()?,
            mac: String::new(),
            link_local: Vec::new(),
            ip,
            ip6,
            dns_names,
        };
        let mut members = lock(&self.members);
        let list = members.entry(network.id.clone()).or_default();
        list.retain(|m| m.container != id);
        list.push(member.clone());
        Ok((network, member))
    }

    /// The setup entries and resolv.conf of a run on `network` at `ip`: its address, the
    /// embedded DNS's relay, and the file dockerd writes for a user network
    /// (resolvconf.Generate, internal: nameserver 127.0.0.11, `ndots:0` after the run's
    /// own options, unless they set it). shards writes no comments naming an engine
    /// (D31).
    pub(super) fn network_guest(
        &self,
        network: &Network,
        member: &Member,
        run: &shards_ipc::Run,
        spec: &mut shards_abi::run::Spec,
    ) {
        let Some(pool) = network.pools.first() else { return };
        spec.setup
            .push(format!("address={}/{},{}", member.ip, pool.subnet.1, pool.gateway).into_bytes());
        // Its IPv6 address and gateway, on a network with IPv6 (D99).
        if let (Some(ip6), Some(pool6)) = (member.ip6, network.pools6.first())
            && let Some((_, bits)) = nets::prefix6(&pool6.subnet)
        {
            spec.setup
                .push(format!("address6={ip6}/{bits},{}", pool6.gateway).into_bytes());
        }
        spec.setup.push(format!("dns={}", pool.gateway).into_bytes());
        let mut resolv = String::from("nameserver 127.0.0.11\n");
        if !run.dns_search.is_empty() {
            resolv.push_str(&format!("search {}\n", run.dns_search.join(" ")));
        }
        let mut options = run.dns_options.clone();
        if !options.iter().any(|o| o.starts_with("ndots:")) {
            options.push("ndots:0".into());
        }
        resolv.push_str(&format!("options {}\n", options.join(" ")));
        spec.resolv = Some(resolv.into_bytes());
    }

    /// MicroVM `id`'s network process, on `net`, given its address, a link to each other
    /// member's, and the names they all answer to (D46): before its VM has the run, so
    /// that none of its first frames finds no peer. The other members are each given
    /// their link to it as it is made. Each is answered before the next is asked, and
    /// the daemon's copies of the links close once both have them (M24).
    pub(super) fn give_network(
        &self,
        id: &str,
        net: &std::os::unix::net::UnixStream,
        mac: Option<[u8; 6]>,
        link_local: &[std::net::IpAddr],
    ) -> Result<(), String> {
        let _held = nets::lock();
        let Some((network, mut me)) = self.membership(id) else {
            return Ok(());
        };
        let Some(pool) = network.pools.first() else {
            return Ok(());
        };
        me.mac = mac.map(|m| shards_net::Mac(m).to_string()).unwrap_or_default();
        // Its link-local addresses (PM M175): those given it, and on a network with IPv6
        // the one its kernel makes of its MAC as IPv6 comes on (RFC 4291 Appendix A's
        // modified EUI-64, the kernel's addr_gen_mode 0), which its peers reach it at too.
        me.link_local = link_local.to_vec();
        if let (Some(_), Some([a, b, c, d, e, f])) = (me.ip6, mac) {
            let kernels = Ipv6Addr::from([0xfe, 0x80, 0, 0, 0, 0, 0, 0, a ^ 2, b, c, 0xff, 0xfe, d, e, f]);
            if !me.link_local.contains(&std::net::IpAddr::V6(kernels)) {
                me.link_local.push(std::net::IpAddr::V6(kernels));
            }
        }
        if let Some(list) = lock(&self.members).get_mut(&network.id)
            && let Some(m) = list.iter_mut().find(|m| m.container == id)
        {
            m.mac.clone_from(&me.mac);
            m.link_local.clone_from(&me.link_local);
        }
        let [a, b, c, d] = me.ip.octets();
        let [g0, g1, g2, g3] = pool.gateway.octets();
        let mut address = vec![a, b, c, d, pool.subnet.1, g0, g1, g2, g3];
        // Then its IPv6 address, prefix and gateway, on a network with IPv6.
        if let (Some(ip6), Some(pool6)) = (me.ip6, network.pools6.first())
            && let Some((_, bits)) = nets::prefix6(&pool6.subnet)
            && let Ok(gateway6) = pool6.gateway.parse::<Ipv6Addr>()
        {
            address.extend_from_slice(&ip6.octets());
            address.push(bits);
            address.extend_from_slice(&gateway6.octets());
        }
        ask_net(net, shards_ipc::kind::NET_ADDRESS, &address, &[])?;
        if !me.link_local.is_empty() {
            ask_net(
                net,
                shards_ipc::kind::NET_LINK_LOCAL,
                &shards_net::encode_addresses(&me.link_local),
                &[],
            )?;
        }
        for peer in self
            .members_of(&network.id)
            .into_iter()
            .filter(|m| m.container != id)
        {
            let Some(theirs) = self.net_control(&peer.container) else {
                continue;
            };
            let (ours_end, their_end) =
                std::os::unix::net::UnixStream::pair().map_err(|e| format!("a peer's link: {e}"))?;
            use std::os::fd::AsFd as _;
            // Each peer's address, its IPv6 one's length (0 or 16) and that address, then
            // its link-local addresses.
            let addresses = |m: &Member| -> Vec<u8> {
                let mut v = m.ip.octets().to_vec();
                match m.ip6 {
                    Some(a) => {
                        v.push(16);
                        v.extend(a.octets());
                    }
                    None => v.push(0),
                }
                v.extend(shards_net::encode_addresses(&m.link_local));
                v
            };
            ask_net(
                net,
                shards_ipc::kind::NET_PEER,
                &addresses(&peer),
                &[ours_end.as_fd()],
            )?;
            // A peer that cannot take it has gone; its own end comes soon.
            let _ = ask_net(
                &theirs,
                shards_ipc::kind::NET_PEER,
                &addresses(&me),
                &[their_end.as_fd()],
            );
        }
        // Its own first, whose run is not yet followed, then the rest.
        ask_net(net, shards_ipc::kind::NET_NAMES, &self.names_table(&network), &[])?;
        self.tell_names(&network);
        Ok(())
    }

    /// A copy of microVM `id`'s network process's control socket, while it runs.
    fn net_control(&self, id: &str) -> Option<std::os::unix::net::UnixStream> {
        match lock(&self.runs).get(id) {
            Some(super::RunState::Tracked(t)) => t.net.as_ref()?.try_clone().ok(),
            _ => None,
        }
    }

    /// The names network `network`'s members answer to, as `NET_NAMES` carries them.
    fn names_table(&self, network: &Network) -> Vec<u8> {
        let mut entries = Vec::new();
        for m in &self.members_of(&network.id) {
            let mut names = m.dns_names.clone();
            if !m.name.is_empty() && !names.contains(&m.name) {
                names.insert(0, m.name.clone());
            }
            for n in names {
                entries.push((n.clone(), std::net::IpAddr::V4(m.ip)));
                if let Some(ip6) = m.ip6 {
                    entries.push((n, std::net::IpAddr::V6(ip6)));
                }
            }
        }
        shards_net::dns::Names::encode(&network.name, &entries)
    }

    /// Network `network`'s names, to each followed member's network process.
    pub(super) fn tell_names(&self, network: &Network) {
        let table = self.names_table(network);
        for m in &self.members_of(&network.id) {
            if let Some(net) = self.net_control(&m.container) {
                let _ = ask_net(&net, shards_ipc::kind::NET_NAMES, &table, &[]);
            }
        }
    }

    /// MicroVM `id`'s run has ended: no longer a member, and its peers' names say so.
    pub(super) fn leave_network(&self, id: &str) {
        let left: Vec<String> = {
            let mut members = lock(&self.members);
            let mut left = Vec::new();
            for (net, list) in members.iter_mut() {
                if list.iter().any(|m| m.container == id) {
                    list.retain(|m| m.container != id);
                    left.push(net.clone());
                }
            }
            left
        };
        let networks = self.all_networks();
        for net in left {
            if let Some(n) = networks.iter().find(|n| n.id == net) {
                let _held = nets::lock();
                self.tell_names(n);
            }
        }
    }

    /// Every IPv4 pool allocated, the default bridge's among them.
    fn allocated(&self, networks: &[Network]) -> Vec<nets::Pool> {
        networks.iter().flat_map(|n| n.pools.iter().cloned()).collect()
    }

    /// `shards network create NAME` (NetworkCreate, Controller.NewNetwork): the CLI's
    /// checks, then dockerd's, each in its order and words; its ID once kept.
    pub(super) fn network_create(&self, parsed: &shards_cmdline::flags::Parsed, reply: &Reply<'_>) -> u8 {
        let Some(name) = parsed.args.first() else {
            return 1;
        };
        let daemon = |e: &str| {
            reply.err(&format!("Error response from daemon: {e}"));
            1
        };
        let aux = kv_map(parsed.many("aux-address"));
        let configs = match consolidate(
            parsed.many("subnet"),
            parsed.many("ip-range"),
            parsed.many("gateway"),
            &aux,
        ) {
            Ok(c) => c,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        if nets::PREDEFINED.contains(&name.as_str()) {
            return daemon(&format!(
                "operation is not permitted on predefined {name} network "
            ));
        }
        let driver = match parsed.string("driver") {
            "" => "bridge",
            d => d,
        };
        if driver == "overlay" {
            return daemon(
                "This node is not a swarm manager. Use \"docker swarm init\" or \"docker swarm join\" to connect this node to swarm and try again.",
            );
        }
        let mut options = kv_map(parsed.many("opt"));
        let config_from = parsed.string("config-from").to_string();
        let family = |flag: &str, option: &str, default: bool, options: &mut BTreeMap<String, String>| {
            if parsed.changed(flag) {
                options.remove(option);
                return Ok(parsed.bool(flag));
            }
            match options.get(option) {
                Some(v) => shards_cmdline::go::parse_bool(v).map_err(|_| {
                    format!(
                        "driver-opt {} is not a valid bool",
                        shards_cmdline::go::quote(option)
                    )
                }),
                None => Ok(default),
            }
        };
        let ipv4 = match family("ipv4", "com.docker.network.enable_ipv4", true, &mut options) {
            Ok(b) => b,
            Err(e) => return daemon(&e),
        };
        let ipv6 = match family("ipv6", "com.docker.network.enable_ipv6", false, &mut options) {
            Ok(b) => b,
            Err(e) => return daemon(&e),
        };
        // IPv6's configs go where IPv6 is off.
        let configs: Vec<Asked> = configs
            .into_iter()
            .filter(|c| ipv6 || netip_prefix(&c.subnet).is_ok_and(|(ip, _)| ip.is_ipv4()))
            .collect();
        let errs: Vec<String> = configs.iter().flat_map(ipam_errors).collect();
        if !errs.is_empty() {
            return daemon(&joined("invalid network config:", &errs));
        }
        if name.trim().is_empty() {
            return daemon("invalid name: name is empty");
        }
        let _held = nets::lock();
        let networks = self.all_networks();
        if networks.iter().any(|n| &n.name == name) {
            return daemon(&format!("network with name {name} already exists"));
        }
        let config_only = parsed.bool("config-only");
        let (driver, scope) = if config_only {
            ("null", "local".to_string())
        } else {
            (driver, parsed.string("scope").to_string())
        };
        let mut from: Option<Network> = None;
        if !config_from.is_empty() {
            let Some(source) = networks.iter().find(|n| n.name == config_from && n.config_only) else {
                return daemon(&format!(
                    "configuration network {} does not exist",
                    shards_cmdline::go::quote(&config_from)
                ));
            };
            if config_only {
                return daemon("a configuration network cannot depend on another configuration network");
            }
            if !configs.is_empty() {
                return daemon(
                    "user-specified configurations are not supported if the network depends on a configuration network",
                );
            }
            if !options.is_empty() {
                return daemon(
                    "network driver options are not supported if the network depends on a configuration network",
                );
            }
            from = Some(source.clone());
        }
        match driver {
            "bridge" | "null" | "macvlan" | "ipvlan" | "host" => {}
            other => return daemon(&format!("plugin {} not found", shards_cmdline::go::quote(other))),
        }
        if !config_only && (driver == "host" || (driver == "null" && !config_only)) {
            let kind = if driver == "host" { "host" } else { "null" };
            return daemon(&format!(
                "only one instance of {} network is allowed",
                shards_cmdline::go::quote(kind)
            ));
        }
        let ipam_driver = parsed.string("ipam-driver").to_string();
        if !matches!(ipam_driver.as_str(), "default" | "") {
            return daemon(&format!(
                "plugin {} not found",
                shards_cmdline::go::quote(&ipam_driver)
            ));
        }
        if scope == "swarm" {
            return daemon("cannot create a swarm scoped network when swarm is not active");
        }
        if parsed.bool("ingress") {
            return daemon("Ingress network can only be global scope network");
        }
        if !ipv4 && !ipv6 {
            return daemon("IPv4 or IPv6 must be enabled");
        }
        let Ok(id) = new_id() else {
            return daemon("making the network's ID");
        };
        let mut options = from.as_ref().map_or(options, |f| f.options.clone());
        let configs = from.as_ref().map_or(configs, |f| {
            f.pools
                .iter()
                .map(|p| Asked {
                    subnet: nets::show(p.subnet),
                    ip_range: p.ip_range.map(nets::show).unwrap_or_default(),
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
                return daemon("bridge driver doesn't support multiple subnets");
            }
            let allocated: Vec<shards_net::bridge::Prefix> =
                self.allocated(&networks).iter().map(|p| p.subnet).collect();
            let asked = v4.first();
            let explicit = asked.and_then(|c| nets::parse_prefix(&c.subnet));
            // Explicit pools are checked against the networks' alone; the daemon's own
            // picks also keep off the host's (InferReservedNetworks).
            let mut taken = allocated.clone();
            if explicit.is_none_or(|(a, _)| a.is_unspecified()) {
                taken.extend(
                    shards_net::bridge::reserved(&shards_net::bridge::host_resolv()).unwrap_or_default(),
                );
            }
            let range = asked.and_then(|c| nets::parse_prefix(&c.ip_range));
            let gateway = asked.and_then(|c| c.gateway.parse().ok());
            let aux: BTreeMap<String, Ipv4Addr> = asked
                .map(|c| {
                    c.aux
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.parse().ok()?)))
                        .collect()
                })
                .unwrap_or_default();
            match nets::new_pool(explicit, range, gateway, &aux, &taken) {
                Ok(p) => pools.push(p),
                Err(e) => return daemon(&e),
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
                // The ULA pool dockerd derives from its host's ID: fd00::/8 with the ID's
                // hash's 40 bits, /48, given out a /64 at a time.
                use sha2::Digest as _;
                let hash = sha2::Sha256::digest(self.engine_id().as_bytes());
                let gid = u64::from_be_bytes(hash.get(..8).and_then(|b| b.try_into().ok()).unwrap_or([0; 8]))
                    & ((1u64 << 40) - 1);
                let base = (0xfd00u128 << 112) | (u128::from(gid) << 80);
                let used: Vec<String> = networks
                    .iter()
                    .flat_map(|n| n.pools6.iter().map(|p| p.subnet.clone()))
                    .collect();
                let subnet = (0u128..1 << 16)
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
        // The bridge driver's own options.
        if driver == "bridge" && !config_only {
            if let Some(mtu) = options.get("com.docker.network.driver.mtu")
                && mtu.parse::<i64>().is_err()
            {
                return daemon(&format!(
                    "failed to parse com.docker.network.driver.mtu value: {mtu} (strconv.Atoi: parsing {}: invalid syntax)",
                    shards_cmdline::go::quote(mtu)
                ));
            }
            if let Some(icc) = options.get("com.docker.network.bridge.enable_icc")
                && shards_cmdline::go::parse_bool(icc).is_err()
            {
                return daemon(&format!(
                    "failed to parse com.docker.network.bridge.enable_icc value: {icc} (strconv.ParseBool: parsing {}: invalid syntax)",
                    shards_cmdline::go::quote(icc)
                ));
            }
            let bridge_name = |n: &Network| {
                n.options
                    .get("com.docker.network.bridge.name")
                    .cloned()
                    .unwrap_or_else(|| format!("br-{}", n.id.get(..12).unwrap_or(&n.id)))
            };
            let mine = options
                .get("com.docker.network.bridge.name")
                .cloned()
                .unwrap_or_else(|| format!("br-{}", id.get(..12).unwrap_or(&id)));
            if mine.len() > 15 {
                return daemon("numerical result out of range");
            }
            if let Some(other) = networks
                .iter()
                .find(|n| n.driver == "bridge" && !n.config_only && bridge_name(n) == mine)
            {
                return daemon(&format!(
                    "cannot create network {id} ({mine}): conflicts with network {} ({mine}): networks have same bridge name",
                    other.id
                ));
            }
        }
        if from.is_some() {
            options.clear();
        }
        let n = Network {
            name: name.clone(),
            id: id.clone(),
            created: crate::containers::now(),
            scope: if scope.is_empty() { "local".into() } else { scope },
            driver: driver.into(),
            ipv4: ipv4 && !config_only,
            ipv6: ipv6 && !config_only,
            ipam_driver: "default".into(),
            ipam_options: kv_map(parsed.many("ipam-opt")),
            pools,
            pools6,
            v6_first: v4.is_empty() && !v6.is_empty(),
            internal: parsed.bool("internal"),
            attachable: parsed.bool("attachable"),
            ingress: false,
            config_from,
            config_only,
            options: from.map_or(options, |f| f.options),
            labels: kv_map(parsed.many("label")),
        };
        if let Err(e) = Store::new(&self.home).put(&n) {
            return daemon(&e);
        }
        reply.out(&id);
        0
    }

    /// Network `n` as dockerd's network.Inspect says it.
    pub(super) fn network_value(&self, n: &Network) -> Value {
        let members = self.members_of(&n.id);
        let map = |m: &BTreeMap<String, String>| Value::string_map(m.clone());
        let special = matches!(n.driver.as_str(), "host" | "null");
        let entry = |subnet: String, range: String, gateway: String, aux: BTreeMap<String, String>| {
            Struct::new("network.IPAMConfig")
                .tagged("Subnet", Some("Subnet"), true, Value::String(subnet))
                .tagged("IPRange", Some("IPRange"), true, Value::String(range))
                .tagged("Gateway", Some("Gateway"), true, Value::String(gateway))
                .tagged(
                    "AuxiliaryAddresses",
                    Some("AuxiliaryAddresses"),
                    true,
                    if aux.is_empty() {
                        Value::NilMap(Kind::String)
                    } else {
                        Value::string_map(aux)
                    },
                )
                .value()
        };
        let v4: Vec<Value> = n
            .pools
            .iter()
            .map(|p| {
                entry(
                    nets::show(p.subnet),
                    p.ip_range.map(nets::show).unwrap_or_default(),
                    p.gateway.to_string(),
                    p.aux.iter().map(|(k, v)| (k.clone(), v.to_string())).collect(),
                )
            })
            .collect();
        let v6: Vec<Value> = n
            .pools6
            .iter()
            .map(|p| {
                entry(
                    p.subnet.clone(),
                    p.ip_range.clone(),
                    p.gateway.clone(),
                    p.aux.clone(),
                )
            })
            .collect();
        let config = if special || n.config_only {
            Value::NilList(Kind::Any)
        } else if n.v6_first {
            Value::List(Kind::Any, v6.into_iter().chain(v4).collect())
        } else {
            Value::List(Kind::Any, v4.into_iter().chain(v6).collect())
        };
        let ipam_options = if n.predefined() {
            Value::NilMap(Kind::String)
        } else {
            map(&n.ipam_options)
        };
        let containers: BTreeMap<String, Value> = members
            .iter()
            .map(|m| {
                let prefix = n
                    .pools
                    .iter()
                    .find(|p| nets::contains(p.subnet, m.ip))
                    .map_or(32, |p| p.subnet.1);
                (
                    m.container.clone(),
                    Struct::new("network.EndpointResource")
                        .field("Name", Value::String(m.name.clone()))
                        .field("EndpointID", Value::String(m.endpoint.clone()))
                        .field("MacAddress", Value::String(m.mac.clone()))
                        .field("IPv4Address", Value::String(format!("{}/{prefix}", m.ip)))
                        .field(
                            "IPv6Address",
                            Value::String(
                                m.ip6
                                    .zip(n.pools6.first().and_then(|p| nets::prefix6(&p.subnet)))
                                    .map(|(a, (_, bits))| format!("{a}/{bits}"))
                                    .unwrap_or_default(),
                            ),
                        )
                        .value(),
                )
            })
            .collect();
        let ips: Vec<Ipv4Addr> = members.iter().map(|m| m.ip).collect();
        let mut subnets: BTreeMap<String, Value> = BTreeMap::new();
        if !special && !n.config_only {
            for p in &n.pools {
                let (used, free) = nets::counts(p, &ips);
                subnets.insert(
                    nets::show(p.subnet),
                    Struct::new("network.SubnetStatus")
                        .field("IPsInUse", Value::Uint(used))
                        .field("DynamicIPsAvailable", Value::Uint(free))
                        .value(),
                );
            }
            for p in &n.pools6 {
                let bits: u32 = p
                    .ip_range
                    .split_once('/')
                    .or_else(|| p.subnet.split_once('/'))
                    .and_then(|(_, b)| b.parse().ok())
                    .unwrap_or(128);
                let host = 128u32.saturating_sub(bits);
                let size = if host >= 64 { u64::MAX } else { 1u64 << host };
                // The network's address and the gateway are marked.
                subnets.insert(
                    p.subnet.clone(),
                    Struct::new("network.SubnetStatus")
                        .field("IPsInUse", Value::Uint(2))
                        .field(
                            "DynamicIPsAvailable",
                            Value::Uint(size.saturating_sub(if host >= 64 { 1 } else { 2 })),
                        )
                        .value(),
                );
            }
        }
        let status_ipam = if subnets.is_empty() {
            Struct::new("network.IPAMStatus")
                .tagged("Subnets", Some("Subnets"), true, Value::NilMap(Kind::Any))
                .value()
        } else {
            Struct::new("network.IPAMStatus")
                .tagged("Subnets", Some("Subnets"), true, Value::Map(Kind::Any, subnets))
                .value()
        };
        Struct::new("network.Inspect")
            .field("Name", Value::String(n.name.clone()))
            .field("Id", Value::String(n.id.clone()))
            .field("Created", Value::String(rfc3339_nano(n.created)))
            .field("Scope", Value::String(n.scope.clone()))
            .field("Driver", Value::String(n.driver.clone()))
            .field("EnableIPv4", Value::Bool(n.ipv4))
            .field("EnableIPv6", Value::Bool(n.ipv6))
            .field(
                "IPAM",
                Struct::new("network.IPAM")
                    .field("Driver", Value::String(n.ipam_driver.clone()))
                    .field("Options", ipam_options)
                    .field("Config", config)
                    .value(),
            )
            .field("Internal", Value::Bool(n.internal))
            .field("Attachable", Value::Bool(n.attachable))
            .field("Ingress", Value::Bool(n.ingress))
            .field(
                "ConfigFrom",
                Struct::new("network.ConfigReference")
                    .field("Network", Value::String(n.config_from.clone()))
                    .value(),
            )
            .field("ConfigOnly", Value::Bool(n.config_only))
            .field("Options", map(&n.options))
            .field("Labels", map(&n.labels))
            .field("Containers", Value::Map(Kind::Any, containers))
            .tagged(
                "Status",
                Some("Status"),
                true,
                Struct::new("network.Status").field("IPAM", status_ipam).value(),
            )
            .value()
    }

    /// `shards network inspect NETWORK...` (GET /networks/{id}): each found, in order;
    /// each not, said after.
    pub(super) fn network_inspect(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        let networks = self.all_networks();
        let (mut documents, mut errors) = (Vec::new(), Vec::new());
        for term in &parsed.args {
            match nets::find(&networks, term) {
                Found::One(n) => documents.push(self.network_value(n)),
                Found::Ambiguous(count, by_name) => errors.push(format!(
                    "Error response from daemon: {count} matches found based on {}: network {term} is ambiguous",
                    if by_name { "name" } else { "ID prefix" }
                )),
                Found::None => errors.push(format!("Error response from daemon: network {term} not found")),
            }
        }
        super::inspect::inspected(parsed.string("format"), &documents, errors, asker.styled(), reply)
    }

    /// Whether network `n` holds no member (dangling).
    fn idle(&self, n: &Network) -> bool {
        self.members_of(&n.id).is_empty()
    }

    /// `shards network ls` (NetworkList, daemon/network/filter.go): the networks as data,
    /// for the client to lay out as the CLI does.
    pub(super) fn network_ls(&self, parsed: &shards_cmdline::flags::Parsed, reply: &Reply<'_>) -> u8 {
        let filters = Filters::from_flags(parsed.many("filter"));
        let listed = filters
            .validate(&LIST_FILTERS)
            .and_then(|()| self.networks_by(&filters));
        let networks = match listed {
            Ok(n) => n,
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let rows: Vec<serde_json::Value> = networks
            .iter()
            .map(|n| {
                serde_json::json!({
                    "Name": n.name, "Id": n.id, "Created": n.created.to_string(), "Driver": n.driver,
                    "Scope": n.scope, "EnableIPv4": n.ipv4, "EnableIPv6": n.ipv6,
                    "Internal": n.internal, "Labels": n.labels,
                })
            })
            .collect();
        let mut sheet = shards_ipc::Sheet::new("networks-rows");
        sheet.record(&[("rows", serde_json::Value::Array(rows).to_string())]);
        reply.sheet(&sheet);
        0
    }

    /// The networks `filters` keep, in dockerd's terms and words.
    fn networks_by(&self, filters: &Filters) -> Result<Vec<Network>, String> {
        let kind = match filters.get("type").next() {
            None => None,
            Some(t @ ("builtin" | "custom")) => Some(t == "builtin"),
            Some(t) => return Err(format!("invalid filter: 'type'='{t}'")),
        };
        let dangling = if filters.contains("dangling") {
            let values: Vec<&str> = filters.get("dangling").collect();
            if values.len() > 1 {
                return Err("got more than one value for filter key \"dangling\"".into());
            }
            match values.first().copied() {
                Some("true" | "1") => Some(true),
                Some("false" | "0") => Some(false),
                _ => {
                    return Err(
                        "invalid value for filter 'dangling', must be \"true\" (or \"1\"), or \"false\" (or \"0\")".into(),
                    );
                }
            }
        } else {
            None
        };
        let (names, ids) = (filters.matcher("name"), filters.matcher("id"));
        Ok(self
            .all_networks()
            .into_iter()
            .filter(|n| filters.exact("driver", &n.driver) && filters.exact("scope", &n.scope))
            .filter(|n| names.matches(&n.name) && ids.matches(&n.id))
            .filter(|n| filters.kv("label", &n.labels))
            .filter(|n| kind.is_none_or(|builtin| n.predefined() == builtin))
            .filter(|n| dangling.is_none_or(|d| (!n.predefined() && self.idle(n)) == d))
            .collect())
    }

    /// `shards network rm NETWORK...` (DELETE /networks/{id}): each removed said as given;
    /// each refused said as it comes, then `exit status 1`, as the CLI says them.
    pub(super) fn network_rm(&self, parsed: &shards_cmdline::flags::Parsed, reply: &Reply<'_>) -> u8 {
        let mut failed = false;
        for term in &parsed.args {
            let _held = nets::lock();
            let networks = self.all_networks();
            let said = match nets::find(&networks, term) {
                Found::One(n) if n.predefined() => Err(format!(
                    "{} is a pre-defined network and cannot be removed",
                    n.name
                )),
                Found::One(n) => {
                    let members = self.members_of(&n.id);
                    if !members.is_empty() {
                        let listed: Vec<String> = members
                            .iter()
                            .map(|m| {
                                format!(
                                    "name:{} id:{}",
                                    shards_cmdline::go::quote(&m.name),
                                    shards_cmdline::go::quote(m.endpoint.get(..12).unwrap_or(&m.endpoint))
                                )
                            })
                            .collect();
                        Err(format!(
                            "error while removing network: network {} has active endpoints ({})",
                            n.name,
                            listed.join(", ")
                        ))
                    } else if n.config_only && networks.iter().any(|o| o.config_from == n.name) {
                        Err(format!(
                            "error while removing network: configuration network {} is in use",
                            shards_cmdline::go::quote(&n.name)
                        ))
                    } else {
                        Store::new(&self.home).remove(&n.id)
                    }
                }
                Found::Ambiguous(count, by_name) => Err(format!(
                    "network {term} is ambiguous ({count} matches found based on {})",
                    if by_name { "name" } else { "ID prefix" }
                )),
                Found::None if parsed.bool("force") => continue,
                Found::None => Err(format!("network {term} not found")),
            };
            match said {
                Ok(()) => reply.out(term),
                Err(e) => {
                    reply.err(&format!("Error response from daemon: {e}"));
                    failed = true;
                }
            }
        }
        if failed {
            reply.err("exit status 1");
            return 1;
        }
        0
    }

    /// `shards network prune` (NetworksPrune): every user network with no member that the
    /// filters keep, named as it goes.
    pub(super) fn network_prune(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        match self.prune_networks(parsed.many("filter"), asker) {
            Ok(names) => {
                if !names.is_empty() {
                    reply.out(&format!("Deleted Networks:\n{}\n", names.join("\n")));
                }
                0
            }
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                1
            }
        }
    }

    /// The user networks prune takes (`filters`: label, label!, until), removed; their
    /// names.
    pub(super) fn prune_networks(&self, given: &[String], asker: &Asker) -> Result<Vec<String>, String> {
        let filters = Filters::from_flags(given);
        filters.validate(&PRUNE_FILTERS)?;
        let untils: Vec<&str> = filters.get("until").collect();
        if untils.len() > 1 {
            return Err("more than one until filter specified".into());
        }
        let until = match untils.first() {
            Some(v) => Some(shards_cmdline::gotime::parse_timestamp(
                v,
                i128::from(asker.now),
                i64::from(asker.utc_offset),
            )?),
            None => None,
        };
        let _held = nets::lock();
        let mut names = Vec::new();
        for n in self.all_networks() {
            let keep = n.predefined()
                || n.config_only
                || !self.idle(&n)
                || !filters.kv("label", &n.labels)
                || (filters.contains("label!") && filters.kv("label!", &n.labels))
                || until.is_some_and(|u| i128::try_from(n.created).unwrap_or(i128::MAX) > u);
            if keep {
                continue;
            }
            if Store::new(&self.home).remove(&n.id).is_ok() {
                names.push(n.name);
            }
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// createIPAMConfig's words, as docker/cli 29.3.1 printed them (shards-dind).
    #[test]
    fn ipam_configs_are_consolidated_as_the_cli_consolidates_them() {
        let none = BTreeMap::new();
        assert_eq!(
            consolidate(&[], &[], &s(&["10.0.0.1"]), &none).unwrap_err(),
            "every ip-range or gateway must have a corresponding subnet"
        );
        assert_eq!(
            consolidate(&s(&["10.10.0.0/16", "10.10.1.0/24"]), &[], &[], &none).unwrap_err(),
            "multiple overlapping subnet configuration is not supported"
        );
        assert_eq!(
            consolidate(&s(&["bogus"]), &[], &[], &none).unwrap_err(),
            "netip.ParsePrefix(\"bogus\"): no '/'"
        );
        assert_eq!(
            consolidate(&s(&["10.10.0.0/33"]), &[], &[], &none).unwrap_err(),
            "netip.ParsePrefix(\"10.10.0.0/33\"): prefix length out of range"
        );
        assert_eq!(
            consolidate(
                &s(&["10.10.0.0/16", "10.20.0.0/16"]),
                &s(&["10.10.1.0/24", "10.10.0.0/24"]),
                &[],
                &none
            )
            .unwrap_err(),
            "cannot configure multiple ranges (10.10.0.0/24, 10.10.1.0/24) on the same subnet (10.10.0.0/16)"
        );
        assert_eq!(
            consolidate(&s(&["10.10.0.0/16"]), &[], &s(&["10.20.0.1"]), &none).unwrap_err(),
            "no matching subnet for gateway 10.20.0.1"
        );
        let aux = BTreeMap::from([("a".to_string(), "bogus".to_string())]);
        assert_eq!(
            consolidate(&s(&["10.10.0.0/16"]), &[], &[], &aux).unwrap_err(),
            "ParseAddr(\"bogus\"): unable to parse IP"
        );
        let c = consolidate(&s(&["10.10.0.5/16"]), &[], &[], &none).unwrap();
        assert_eq!(
            ipam_errors(&c[0]),
            ["invalid subnet 10.10.0.5/16: it should be 10.10.0.0/16"]
        );
        let c = consolidate(&s(&["10.10.0.0/24"]), &s(&["10.10.0.0/16"]), &[], &none).unwrap();
        assert_eq!(
            ipam_errors(&c[0]),
            ["invalid ip-range 10.10.0.0/16: CIDR block is bigger than its parent subnet 10.10.0.0/24"]
        );
        assert_eq!(
            rfc3339_nano(1_791_260_014_360_799_172),
            "2026-10-06T04:13:34.360799172Z"
        );
    }
}
