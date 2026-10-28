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
    net: &crate::netproc::Control,
    which: u8,
    payload: &[u8],
    fds: &[crate::netproc::Lent<'_>],
) -> Result<(), String> {
    net.set_read_timeout(Some(super::PUBLISH_PATIENCE))
        .map_err(|e| e.to_string())?;
    crate::netproc::send(net, which, payload, fds).map_err(|e| format!("the network process: {e}"))?;
    match crate::netproc::recv(net) {
        Ok(Some(m)) if m.kind == which => Ok(()),
        Ok(_) => Err("the network process did not take it".into()),
        Err(e) => Err(format!("the network process: {e}")),
    }
}

/// A peer link's end as a socket of its own, to hand to a network process and hold until
/// it says it has it: a Unix socket's descriptor; on Windows an AF_UNIX socket, which that
/// process waits on with its own (PM M213).
fn owned(end: shards_vmm::platform::UnixStream) -> crate::netproc::Sock {
    end.into()
}

/// What a warm VM's network process is told before the VM has its run (D31, E1): each
/// ask sent as it comes, and their answers read after the last, so that all of them cost
/// one round trip, not one each. Each answer says which ask it is by its kind, and comes
/// in the order asked. The descriptors an ask hands over are kept until its answer has
/// come: XNU drops a socket in flight that no process holds (M24).
pub(super) struct Asks<'n> {
    net: &'n crate::netproc::Control,
    /// Each ask's kind, and what it is, to say where it is not taken.
    asked: Vec<(u8, &'static str)>,
    held: Vec<crate::netproc::Sock>,
}

impl<'n> Asks<'n> {
    pub(super) fn new(net: &'n crate::netproc::Control) -> Result<Asks<'n>, String> {
        net.set_read_timeout(Some(super::PUBLISH_PATIENCE))
            .map_err(|e| format!("network process: {e}"))?;
        Ok(Asks {
            net,
            asked: Vec::new(),
            held: Vec::new(),
        })
    }

    /// Sends `which`, `what` it is, with `payload` and `fds`, which the caller keeps
    /// until [`Asks::answered`].
    pub(super) fn ask(
        &mut self,
        what: &'static str,
        which: u8,
        payload: &[u8],
        fds: &[crate::netproc::Lent<'_>],
    ) -> Result<(), String> {
        crate::netproc::send(self.net, which, payload, fds)
            .map_err(|e| format!("{what}: the network process: {e}"))?;
        self.asked.push((which, what));
        Ok(())
    }

    /// As [`Asks::ask`], the descriptors handed over held here until answered.
    pub(super) fn ask_owning(
        &mut self,
        what: &'static str,
        which: u8,
        payload: &[u8],
        fds: Vec<crate::netproc::Sock>,
    ) -> Result<(), String> {
        let borrowed: Vec<crate::netproc::Lent<'_>> = fds.iter().map(crate::netproc::lend).collect();
        self.ask(what, which, payload, &borrowed)?;
        self.held.extend(fds);
        Ok(())
    }

    /// Each ask's answer, in the order asked: the first not taken says which.
    pub(super) fn answered(self) -> Result<(), String> {
        for (which, what) in &self.asked {
            match crate::netproc::recv(self.net) {
                Ok(Some(m)) if m.kind == *which => {}
                Ok(_) => return Err(format!("{what}: the network process did not take it")),
                Err(e) => return Err(format!("{what}: the network process: {e}")),
            }
        }
        Ok(())
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
        (nets::DEFAULT_BRIDGE, "true".to_string()),
        (nets::ICC, "false".to_string()),
        (nets::MASQUERADE, "true".to_string()),
        (nets::HOST_BINDING, "0.0.0.0".to_string()),
        (nets::MTU, shards_vmm::devices::virtio::net::MTU.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

impl<D: Disk> Daemon<D> {
    /// The ULA pool dockerd derives from its host's ID: fd00::/8 with the ID's hash's 40
    /// bits, a /48.
    fn ula_base(&self) -> u128 {
        use sha2::Digest as _;
        let hash = sha2::Sha256::digest(self.engine_id().as_bytes());
        let gid = u64::from_be_bytes(hash.get(..8).and_then(|b| b.try_into().ok()).unwrap_or([0; 8]))
            & ((1u64 << 40) - 1);
        (0xfd00u128 << 112) | (u128::from(gid) << 80)
    }

    /// A run on the host's network's IPv6 address and its gateway's (D124): in the pool's
    /// last /64, which no user network is given, `::2` and `::1`, each run's link to its
    /// network process its own. What it sends past its loopback is the host's by them.
    pub(super) fn host_ipv6(&self) -> (Ipv6Addr, Ipv6Addr) {
        let subnet = self.ula_base() | (0xffff << 64);
        (Ipv6Addr::from(subnet | 2), Ipv6Addr::from(subnet | 1))
    }

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
                    let Ok(id) = new_id() else { continue };
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
        let id = self
            .resolve(given)
            .map_err(|e| e.strip_prefix("shards: ").unwrap_or(&e).to_string())?;
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
            reply.err(&format!("shards: {e}"));
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
                    "{term} names {count} networks by {}: give more of its ID",
                    if by_name { "name" } else { "their IDs' beginning" }
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
                return daemon(&format!("no network is named {term}"));
            };
            if connected(&run) {
                let cname = lock(&self.containers)
                    .get(&id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                return daemon(&format!(
                    "{} is on network {} already",
                    cname.trim_start_matches('/'),
                    n.name
                ));
            }
            return daemon(
                "connecting a running microVM to another network is not supported by shards yet: it has one network device",
            );
        }
        if name == "host" {
            return daemon("a run is on its host's network as it is made, with --network host alone");
        }
        if (name == "none") != (mode == "none") {
            return daemon("a run with no network (--network none) is on no other");
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
                    errs.push("--ip: an address of its own is a run's on a network you made".to_string());
                }
                if !endpoint.aliases.is_empty() {
                    errs.push(
                        "--alias: names are a run's on a network you made, where its peers ask for them"
                            .to_string(),
                    );
                }
            }
            if let Ok(ip) = endpoint.ipv4.parse::<Ipv4Addr>()
                && !n.pools.iter().any(|p| nets::contains(p.subnet, ip))
            {
                errs.push(format!("{ip}: in none of the network's subnets"));
            }
            if !errs.is_empty() {
                return daemon(&errs.join("\n"));
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
            reply.err(&format!("shards: {e}"));
            1
        };
        let networks = self.all_networks();
        let found = match nets::find(&networks, term) {
            Found::One(n) => Some(n.clone()),
            Found::Ambiguous(count, by_name) => {
                return daemon(&format!(
                    "{term} names {count} networks by {}: give more of its ID",
                    if by_name { "name" } else { "their IDs' beginning" }
                ));
            }
            Found::None => None,
        };
        let (id, mut run) = match self.request_of(given) {
            Ok(r) => r,
            Err(_) if parsed.bool("force") => {
                return match found {
                    Some(_) => daemon(&format!("{given} is on no network {term}")),
                    None => daemon(&format!("no network is named {term}")),
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
                return daemon(&format!("no network is named {term}"));
            }
            if mode == "host" && name == "host" {
                return daemon("a run made on its host's network stays on it");
            }
            if self.membership(&id).is_none_or(|(n, _)| n.name != name) && mode != name {
                return daemon(&format!("{given} is not on network {name}"));
            }
            return daemon("disconnecting a running microVM from its network is not supported by shards yet");
        }
        let had = run.endpoints.len();
        run.endpoints.retain(|e| e.network != name);
        let primary = mode == name;
        if run.endpoints.len() == had && !primary {
            return daemon(&format!("{given} is not on network {name}"));
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
        // An asked address held by a run that has ended, its end not yet taken, is free once
        // it is, as dockerd's is once `run` returns: taken first, without the networks'
        // lock, which its leaving takes (PM M182).
        if !asked_ip.is_empty() || !asked_ip6.is_empty() {
            let holders: Vec<String> = {
                let _held = nets::lock();
                self.user_network(term)
                    .map(|n| self.members_of(&n.id))
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|m| {
                        asked_ip.trim_start_matches("::ffff:").parse() == Ok(m.ip)
                            || m.ip6.is_some_and(|ip6| asked_ip6.parse() == Ok(ip6))
                    })
                    .map(|m| m.container)
                    .collect()
            };
            for holder in holders {
                if let Some(inbox) = self.inbox_of(&holder) {
                    self.take_messages(&holder, &inbox);
                }
            }
        }
        let _held = nets::lock();
        let network = self
            .user_network(term)
            .ok_or_else(|| fail(&format!("no network is named {term}")))?;
        let pool = network
            .pools
            .first()
            .cloned()
            .ok_or_else(|| fail(&format!("network {} has no IPv4 subnet", network.name)))?;
        let others: Vec<Ipv4Addr> = self.members_of(&network.id).iter().map(|m| m.ip).collect();
        let ip = if asked_ip.is_empty() {
            nets::allocate(&pool, &others)
                .ok_or_else(|| fail(&format!("network {} has no IPv4 address left", network.name)))?
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
                    nets::allocate6(pool6, &others6)
                        .ok_or_else(|| fail(&format!("network {} has no IPv6 address left", network.name)))?
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

    /// MicroVM `id`'s network process, through `asks`, given its address, a link to each
    /// other member's, and the names they all answer to (D46): before its VM has the run,
    /// so that none of its first frames finds no peer. The other members are each given
    /// their link to it as it is made, each answered before the next is asked: their
    /// processes' sockets are others' to use too. The daemon's copies of the links close
    /// once both have them (M24).
    pub(super) fn give_network(
        &self,
        id: &str,
        asks: &mut Asks<'_>,
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
        asks.ask("network", shards_ipc::kind::NET_ADDRESS, &address, &[])?;
        if !me.link_local.is_empty() {
            asks.ask(
                "network",
                shards_ipc::kind::NET_LINK_LOCAL,
                &shards_net::encode_addresses(&me.link_local),
                &[],
            )?;
        }
        // A link to each other member, where its network lets them reach one another
        // (`enable_icc`); none where it does not, as dockerd's bridge keeps them apart.
        for peer in self
            .members_of(&network.id)
            .into_iter()
            .filter(|m| m.container != id && network.icc())
        {
            let Some(theirs) = self.net_control(&peer.container) else {
                continue;
            };
            let (ours_end, their_end) =
                shards_vmm::platform::UnixStream::pair().map_err(|e| format!("a peer's link: {e}"))?;
            let (ours_end, their_end) = (owned(ours_end), owned(their_end));
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
            asks.ask_owning(
                "network",
                shards_ipc::kind::NET_PEER,
                &addresses(&peer),
                vec![ours_end],
            )?;
            // A peer that cannot take it has gone; its own end comes soon.
            let _ = ask_net(
                &theirs,
                shards_ipc::kind::NET_PEER,
                &addresses(&me),
                &[crate::netproc::lend(&their_end)],
            );
        }
        // Its own, whose run is not yet followed, then the rest's.
        asks.ask(
            "network",
            shards_ipc::kind::NET_NAMES,
            &self.names_table(&network),
            &[],
        )?;
        self.tell_names(&network);
        Ok(())
    }

    /// A copy of microVM `id`'s network process's control socket, while it runs.
    fn net_control(&self, id: &str) -> Option<crate::netproc::Control> {
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
        // A run on no user network: no network to read.
        if left.is_empty() {
            return;
        }
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
    pub(super) fn network_create(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        let Some(name) = parsed.args.first() else {
            return 1;
        };
        let daemon = |e: &str| {
            reply.err(&format!("shards: {e}"));
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
                "{name} is one of shards' own networks, bridge, host and none: give yours another name"
            ));
        }
        let driver = match parsed.string("driver") {
            "" => "bridge",
            d => d,
        };
        if driver == "overlay" {
            return daemon("--driver overlay: shards has no swarm mode, whose networks overlay ones are");
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
            return daemon(&errs.join("\n"));
        }
        if name.trim().is_empty() {
            return daemon("a network needs a name");
        }
        let _held = nets::lock();
        let networks = self.all_networks();
        if networks.iter().any(|n| &n.name == name) {
            return daemon(&format!("a network is named {name} already"));
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
                    "--config-from {config_from}: no config-only network is named so"
                ));
            };
            if config_only {
                return daemon(
                    "--config-from with --config-only: a config-only network takes its settings from none",
                );
            }
            if !configs.is_empty() {
                return daemon(&format!(
                    "--config-from {config_from}: the network's subnets come from it: give none of your own"
                ));
            }
            if !options.is_empty() {
                return daemon(&format!(
                    "--config-from {config_from}: the network's options come from it: give none of your own"
                ));
            }
            from = Some(source.clone());
        }
        match driver {
            "bridge" | "null" | "macvlan" | "ipvlan" | "host" => {}
            other => {
                return daemon(&format!(
                    "--driver {other}: shards' networks are bridge, macvlan, ipvlan, host and null: it has no plugins"
                ));
            }
        }
        if !config_only && (driver == "host" || (driver == "null" && !config_only)) {
            let kind = if driver == "host" { "host" } else { "null" };
            return daemon(&format!("--driver {kind}: shards has one {kind} network already"));
        }
        let ipam_driver = parsed.string("ipam-driver").to_string();
        if !matches!(ipam_driver.as_str(), "default" | "") {
            return daemon(&format!(
                "--ipam-driver {ipam_driver}: shards gives a network's addresses itself (default): it has no plugins"
            ));
        }
        if scope == "swarm" {
            return daemon("--scope swarm: shards has no swarm mode");
        }
        if parsed.bool("ingress") {
            return daemon("--ingress: shards has no swarm mode, whose ingress network it is");
        }
        if !ipv4 && !ipv6 {
            return daemon("--ipv4=false with --ipv6=false: a network needs addresses of one family");
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
                return daemon("a bridge network has one IPv4 subnet in shards, yet");
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
                // The ULA pool dockerd derives from its host's ID, given out a /64 at a
                // time; its last /64 the host's network's (`host_ipv6`).
                let base = self.ula_base();
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
                    nets::MTU => shards_cmdline::go::atoi(value).err().map(|e| e.to_string()),
                    nets::MASQUERADE | nets::ICC | nets::INHIBIT_IPV4 | nets::DEFAULT_BRIDGE => {
                        shards_cmdline::go::parse_bool(value).err().map(|e| e.to_string())
                    }
                    nets::GATEWAY_MODE4 | nets::GATEWAY_MODE6 => {
                        (!matches!(value.as_str(), "nat" | "nat-unprotected" | "routed" | "isolated"))
                            .then(|| format!("unknown gateway mode {value}"))
                    }
                    nets::HOST_BINDING | nets::HOST_IPV4 | nets::HOST_IPV6 => value
                        .parse::<std::net::IpAddr>()
                        .is_err()
                        .then(|| "nil ip".to_string()),
                    _ => None,
                };
                if let Some(why) = why {
                    return daemon(&format!("failed to parse {label} value: {value} ({why})"));
                }
            }
            let isolated = [nets::GATEWAY_MODE4, nets::GATEWAY_MODE6]
                .iter()
                .any(|m| options.get(*m).is_some_and(|v| v == "isolated"));
            if isolated && !parsed.bool("internal") {
                return daemon("gateway mode 'isolated' can only be used for an internal network");
            }
            if let Some(mtu) = options
                .get(nets::MTU)
                .and_then(|v| shards_cmdline::go::atoi(v).ok())
                && mtu < 0
            {
                return daemon(&format!("invalid MTU number: {mtu}"));
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
            .map_or(options.get(nets::EGRESS), |f| f.options.get(nets::EGRESS));
        if let Some(value) = egress {
            if let Err(why) = nets::parse_egress(value) {
                return daemon(&format!("--opt {}: {why}", nets::EGRESS));
            }
            if driver != "bridge" && !config_only {
                return daemon(&format!(
                    "--opt {}: only a bridge network's members reach past their microVMs",
                    nets::EGRESS
                ));
            }
            if parsed.bool("internal") {
                return daemon(&format!(
                    "--opt {}: an internal network's members reach nothing past their microVMs",
                    nets::EGRESS
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
        // An MTU is kept as given, and its microVMs keep their device's: what they send
        // leaves through the host's own sockets, which fit it to each path the host finds,
        // where a container's packets cross the host's wire as they were made. A smaller
        // MTU would only cut their throughput, and have the guest fragment datagrams its
        // network process takes whole (net/src/wire.rs). Said on a terminal; a script
        // reads the ID alone, as dockerd's client prints it.
        if asker.terminal
            && let Some(mtu) = n.options.get(nets::MTU).filter(|m| m.as_str() != "0")
        {
            let text = format!(
                "MTU {mtu} is kept as given: this network's microVMs keep {}, their devices'.\nWhat they send leaves through the host's own sockets, which fit it to each path's MTU.",
                shards_vmm::devices::virtio::net::MTU
            );
            let _ = reply.bytes(
                crate::spec::LOG_STDERR,
                super::push::note(&text, asker.color).as_bytes(),
            );
        }
        0
    }

    /// Network `n` as dockerd's network.Inspect says it.
    pub(super) fn network_value(&self, n: &Network) -> Value {
        self.network_value_of(n, &self.members_of(&n.id))
    }

    /// Network `n` as its inspect document says it, with `members` on it: the members of a
    /// network of a microVM's own engine are that engine's containers (D181).
    pub(super) fn network_value_of(&self, n: &Network, members: &[Member]) -> Value {
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
            .field(
                "Created",
                Value::String(shards_cmdline::format::rfc3339_nano(n.created)),
            )
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
                    "shards: {term} names {count} networks by {}: give more of its ID",
                    if by_name { "name" } else { "their IDs' beginning" }
                )),
                Found::None => errors.push(format!("shards: no network is named {term}")),
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
                reply.err(&format!("shards: {e}"));
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

    /// The networks the filters `given` keep (`KEY=VALUE`, `network ls`'s), each as its
    /// inspect document gives it.
    pub(super) fn networks_listed(&self, given: &[String]) -> Result<Vec<Value>, String> {
        let filters = Filters::from_flags(given);
        filters.validate(&LIST_FILTERS)?;
        Ok(self
            .networks_by(&filters)?
            .iter()
            .map(|n| self.network_value(n))
            .collect())
    }

    /// Network `term`'s inspect document, found by name or ID as `network inspect` finds
    /// it; or why not.
    pub(super) fn network_doc(&self, term: &str) -> Result<Value, String> {
        let networks = self.all_networks();
        match nets::find(&networks, term) {
            Found::One(n) => Ok(self.network_value(n)),
            Found::Ambiguous(count, by_name) => Err(format!(
                "{term} names {count} networks by {}: give more of its ID",
                if by_name { "name" } else { "their IDs' beginning" }
            )),
            Found::None => Err(format!("No such network: {term}")),
        }
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
                Found::One(n) if n.predefined() => {
                    Err(format!("{} is one of shards' own networks, and stays", n.name))
                }
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
                            "cannot remove network {}: runs are on it ({})",
                            n.name,
                            listed.join(", ")
                        ))
                    } else if n.config_only && networks.iter().any(|o| o.config_from == n.name) {
                        Err(format!(
                            "cannot remove network {}: networks take their settings from it",
                            n.name
                        ))
                    } else {
                        Store::new(&self.home).remove(&n.id)
                    }
                }
                Found::Ambiguous(count, by_name) => Err(format!(
                    "{term} names {count} networks by {}: give more of its ID",
                    if by_name { "name" } else { "their IDs' beginning" }
                )),
                Found::None if parsed.bool("force") => continue,
                Found::None => Err(format!("no network is named {term}")),
            };
            match said {
                Ok(()) => reply.out(term),
                Err(e) => {
                    reply.err(&format!("shards: {e}"));
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
        match self.prune_networks(
            parsed.many("filter"),
            i128::from(asker.now),
            i64::from(asker.utc_offset),
        ) {
            Ok(names) => {
                if !names.is_empty() {
                    reply.out(&format!("Deleted Networks:\n{}\n", names.join("\n")));
                }
                0
            }
            Err(e) => {
                reply.err(&format!("shards: {e}"));
                1
            }
        }
    }

    /// The user networks prune takes (`filters`: label, label!, until), removed; their
    /// names.
    pub(super) fn prune_networks(
        &self,
        given: &[String],
        now: i128,
        offset: i64,
    ) -> Result<Vec<String>, String> {
        let filters = Filters::from_flags(given);
        filters.validate(&PRUNE_FILTERS)?;
        let untils: Vec<&str> = filters.get("until").collect();
        if untils.len() > 1 {
            return Err("more than one until filter specified".into());
        }
        let until = match untils.first() {
            Some(v) => Some(shards_cmdline::gotime::parse_timestamp(v, now, offset)?),
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
        // Said as dockerd says each prune (daemon/prune.go): a network's reclaims nothing.
        self.events.log(
            "network",
            "prune",
            "",
            std::collections::BTreeMap::from([("reclaimed".to_string(), "0".to_string())]),
        );
        Ok(names)
    }
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
}
