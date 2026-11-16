//! `shards network` (D46): user-defined networks as dockerd keeps them (moby docker-v29.3.1
//! at f78c987a: daemon/network.go, libnetwork, defaultipam), dockerd's predefined `bridge`,
//! `host` and `none` beside them, each made and removed as `networks::made` makes and removes
//! every network, an orchestrator's among them (D285), with what docker/cli checks before it
//! asks (cli/command/network/create.go createIPAMConfig). Their members are the running
//! microVMs on them, which the daemon holds as it starts and ends their runs, their
//! addresses leased from the home's records, which every process putting runs on a network
//! shares (D285).

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use shards_template::{Kind, Struct, Value};

use super::commands::{Asker, Reply};
use super::filters::Filters;
use super::{Daemon, lock};
use crate::containers::Disk;
use crate::networks::leases::{self, Leases};
use crate::networks::{self as nets, Found, Network, made};

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

/// The daemon's runs on one user network: each member with its addresses' leases, and the
/// network's pairing, held while any of them runs, so that no other process's run joins it
/// meanwhile (D285). Kept only while it has a member: its last's leaving lets the pairing go.
#[derive(Debug)]
pub(super) struct Joined {
    _pairing: leases::Pairing,
    members: Vec<Endpoint>,
}

/// A member, and its addresses' leases, held by the daemon until its run ends.
#[derive(Debug)]
struct Endpoint {
    member: Member,
    _ip: leases::Lease,
    _ip6: Option<leases::Lease<Ipv6Addr>>,
}

/// What a run's start says of a network's refusal of an address or of its pairing, in
/// dockerd's words where it has them (moby f78c987a, daemon/libnetwork/ipamapi/contract.go:35-36,
/// ErrIPAlreadyAllocated and ErrIPOutOfRange), each refusal by its cause.
fn join_refused(network: &Network, family: &str, refused: &leases::Refused) -> String {
    match refused {
        leases::Refused::AddressTaken(_) => "Address already in use".into(),
        leases::Refused::OutOfRange(_) => "requested address is out of range".into(),
        leases::Refused::Exhausted => format!("network {} has no {family} address left", network.name),
        other => format!("network {}: {other}", network.name),
    }
}

/// Sends a network process `which` and waits for it to say it took it, as published
/// ports are given (`give_ports`): while it goes on with it, as D215's judge follows a
/// party ([`crate::netproc::ask`], the daemon's record of its network processes' answers).
pub(super) fn ask_net(
    net: &crate::netproc::NetLink,
    which: u8,
    payload: &[u8],
    fds: &[crate::netproc::Lent<'_>],
) -> Result<(), String> {
    match crate::netproc::ask(net, &crate::netproc::NET_ANSWERS, which, payload, fds) {
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
    net: &'n crate::netproc::NetLink,
    /// Each ask's kind, and what it is, to say where it is not taken.
    asked: Vec<(u8, &'static str)>,
    held: Vec<crate::netproc::Sock>,
    /// The asks' waits, which follow the network process from the first ask's
    /// ([`crate::netproc::until_ready`]), its answers teaching the daemon's record.
    follow: Option<shards_api::Follow<'static>>,
}

impl<'n> Asks<'n> {
    pub(super) fn new(net: &'n crate::netproc::NetLink) -> Result<Asks<'n>, String> {
        Ok(Asks {
            net,
            asked: Vec::new(),
            held: Vec::new(),
            follow: net.follow(&crate::netproc::NET_ANSWERS),
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
        crate::netproc::until_ready(self.net, crate::netproc::Way::Write, &mut self.follow)
            .map_err(|e| format!("{what}: {e}"))?;
        crate::netproc::send(&self.net.control, which, payload, fds)
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

    /// Each ask's answer, in the order asked: the first not taken says which. Each answer
    /// teaches the daemon's record of its network processes' answers.
    pub(super) fn answered(mut self) -> Result<(), String> {
        for (which, what) in &self.asked {
            crate::netproc::until_ready(self.net, crate::netproc::Way::Read, &mut self.follow)
                .map_err(|e| format!("{what}: {e}"))?;
            match crate::netproc::recv(&self.net.control) {
                Ok(Some(m)) if m.kind == *which => {
                    if let Some(follow) = self.follow.as_mut() {
                        follow.answered();
                    }
                }
                Ok(_) => return Err(format!("{what}: the network process did not take it")),
                Err(e) => return Err(format!("{what}: the network process: {e}")),
            }
        }
        Ok(())
    }
}

/// A random 64-hex ID, the form clients parse and abbreviate (D46).
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

/// The names member `asker` of `network` is answered (D46), as `NET_NAMES` carries them:
/// its own, and those of each member it reaches, every member of a network whose members
/// reach one another and none of one made with `enable_icc` false. A name resolves only
/// where its member is reached (default deny): a peer no link reaches has no name either.
fn names_table(network: &Network, members: &[Member], asker: &str) -> Vec<u8> {
    let mut entries = Vec::new();
    for m in members.iter().filter(|m| m.container == asker || network.icc()) {
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

impl<D: Disk> Daemon<D> {
    /// A run on the host's network's IPv6 address and its gateway's, and eth0's IPv6 for an
    /// image's agents' flows past the microVM on a network without it
    /// (`crate::networks::host_ipv6`, D99, D124).
    pub(super) fn host_ipv6(&self) -> (Ipv6Addr, Ipv6Addr) {
        crate::networks::host_ipv6(&self.home)
    }

    /// The networks: dockerd's predefined ones, made here the first time they are
    /// asked for, the bridge's on the subnet this daemon elected, then the user's
    /// (`made::all`).
    pub(super) fn all_networks(&self) -> Vec<Network> {
        made::all(&self.home, self.bridge)
    }

    /// MicroVM `given`'s kept request, its ID and its turn to change it (D37), held until
    /// what changed is kept, for connect and disconnect; dockerd's words where there is none.
    fn request_of(&self, given: &str) -> Result<(super::RequestTurn<'_>, String, shards_ipc::Run), String> {
        let id = self
            .resolve(given)
            .map_err(|e| e.strip_prefix("shards: ").unwrap_or(&e).to_string())?;
        let turn = self.request_turn(&id);
        let dir = lock(&self.containers).dir(&id);
        let run = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .ok_or_else(|| format!("container {id}: its request is not kept"))?;
        Ok((turn, id, run))
    }

    fn keep_request(&self, id: &str, run: &shards_ipc::Run) -> Result<(), String> {
        let dir = lock(&self.containers).dir(id);
        super::keep_whole(&dir.join(super::REQUEST), &run.encode()).map_err(|e| e.to_string())
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
        let (_turn, id, mut run) = match self.request_of(given) {
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
    /// stopped microVM's endpoint on the network gone, so that its next start is not on
    /// it (D46); a running one's, shards does not take away live yet.
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
        let (_turn, id, mut run) = match self.request_of(given) {
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

    /// Network `id`'s members: those whose runs are going, as a member's link and address
    /// last as long as its microVM (D46). One whose run has gone, however its start ended,
    /// is gone.
    pub(super) fn members_of(&self, id: &str) -> Vec<Member> {
        let runs = lock(&self.runs);
        lock(&self.members)
            .get(id)
            .map(|joined| {
                joined
                    .members
                    .iter()
                    .filter(|e| runs.contains_key(&e.member.container))
                    .map(|e| e.member.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// MicroVM `id`'s network and endpoint, while it is a member of one.
    pub(super) fn membership(&self, id: &str) -> Option<(Network, Member)> {
        let networks = self.all_networks();
        lock(&self.members).iter().find_map(|(net, joined)| {
            let e = joined.members.iter().find(|e| e.member.container == id)?;
            Some((networks.iter().find(|n| &n.id == net)?.clone(), e.member.clone()))
        })
    }

    /// MicroVM `id`'s name, as its endpoint's on its network: DNSNames' first.
    pub(super) fn name_member(&self, id: &str, name: &str) {
        for joined in lock(&self.members).values_mut() {
            if let Some(e) = joined.members.iter_mut().find(|e| e.member.container == id) {
                let m = &mut e.member;
                m.name = name.to_string();
                m.dns_names.retain(|n| n != name);
                m.dns_names.insert(0, name.to_string());
            }
        }
    }

    /// MicroVM `id`'s endpoint on user network `term` (its `--ip`, its aliases), made as
    /// it starts (D46): its address the asked one, anywhere in the network's subnet, or the
    /// lowest free in its range, each leased from the home's records and held by the daemon
    /// until its run ends; the network paired by the daemon from its first run's join to its
    /// last's leaving, and refused where another process pairs it (D285). The words scripts
    /// match where the start fails.
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
        // it is, as dockerd's is once `run` returns: taken first, without the home's networks
        // lock, which no caller holds across another process's answer (PM M182, D285).
        if !asked_ip.is_empty() || !asked_ip6.is_empty() {
            let holders: Vec<String> = self
                .user_network(term)
                .map(|n| self.members_of(&n.id))
                .unwrap_or_default()
                .into_iter()
                .filter(|m| {
                    asked_ip.trim_start_matches("::ffff:").parse() == Ok(m.ip)
                        || m.ip6.is_some_and(|ip6| asked_ip6.parse() == Ok(ip6))
                })
                .map(|m| m.container)
                .collect();
            for holder in holders {
                if let Some(inbox) = self.inbox_of(&holder) {
                    self.take_messages(&holder, &inbox);
                }
            }
        }
        // A run joins anew: what a start of it held before is let go first.
        self.leave_network(id);
        let held = leases::hold(&self.home).map_err(|e| fail(&e.to_string()))?;
        let network = self
            .user_network(term)
            .ok_or_else(|| fail(&format!("no network is named {term}")))?;
        let pool = network
            .pools
            .first()
            .cloned()
            .ok_or_else(|| fail(&format!("network {} has no IPv4 subnet", network.name)))?;
        let refused = |family: &str, e: leases::Refused| fail(&join_refused(&network, family, &e));
        let ledger = Leases::of(&self.home, &network.id).map_err(|e| refused("IPv4", e))?;
        let asked = match asked_ip {
            "" => None,
            ip => Some(
                ip.trim_start_matches("::ffff:")
                    .parse::<Ipv4Addr>()
                    .map_err(|_| fail("Address already in use"))?,
            ),
        };
        let lease = ledger
            .take(&held, &pool, asked, id)
            .map_err(|e| refused("IPv4", e))?;
        // Its IPv6 address, as its IPv4 one (D99).
        let lease6 = match (network.ipv6, network.pools6.first()) {
            (true, Some(pool6)) => {
                let asked6 = match asked_ip6 {
                    "" => None,
                    ip6 => Some(
                        ip6.parse::<Ipv6Addr>()
                            .map_err(|_| fail("Address already in use"))?,
                    ),
                };
                Some(
                    ledger
                        .take6(&held, pool6, asked6, id)
                        .map_err(|e| refused("IPv6", e))?,
                )
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
            ip: lease.ip(),
            ip6: lease6.as_ref().map(|l| l.ip()),
            dns_names,
        };
        // Paired with its first run, under the members' lock, which its last's leaving lets
        // the pairing go under: an entry is there while, and only while, the daemon pairs.
        let mut members = lock(&self.members);
        let joined = match members.entry(network.id.clone()) {
            std::collections::btree_map::Entry::Occupied(on) => on.into_mut(),
            std::collections::btree_map::Entry::Vacant(off) => off.insert(Joined {
                _pairing: ledger.pair(&held).map_err(|e| refused("IPv4", e))?,
                members: Vec::new(),
            }),
        };
        joined.members.push(Endpoint {
            member: member.clone(),
            _ip: lease,
            _ip6: lease6,
        });
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
        uplink6: Option<(Ipv6Addr, Ipv6Addr)>,
    ) -> Result<(), String> {
        let _telling = lock(&self.telling);
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
        if let Some(joined) = lock(&self.members).get_mut(&network.id)
            && let Some(m) = joined
                .members
                .iter_mut()
                .map(|endpoint| &mut endpoint.member)
                .find(|m| m.container == id)
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
        } else if let Some((guest6, gateway6)) = uplink6 {
            // Else eth0's IPv6 for its agents' flows past the microVM alone (D99), which no
            // member is told of.
            address.extend_from_slice(&guest6.octets());
            address.push(64);
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
        // (`enable_icc`); none where it does not: no link, no flow (D46).
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
            &names_table(&network, &self.members_of(&network.id), id),
            &[],
        )?;
        self.tell_names(&network);
        Ok(())
    }

    /// A copy of microVM `id`'s network process's control socket, while it runs.
    fn net_control(&self, id: &str) -> Option<crate::netproc::NetLink> {
        match lock(&self.runs).get(id) {
            Some(super::RunState::Tracked(t)) => t.net.as_ref()?.try_clone().ok(),
            _ => None,
        }
    }

    /// Network `network`'s names, to each followed member's network process: each its own
    /// table ([`names_table`]).
    pub(super) fn tell_names(&self, network: &Network) {
        let members = self.members_of(&network.id);
        for m in &members {
            if let Some(net) = self.net_control(&m.container) {
                let table = names_table(network, &members, &m.container);
                let _ = ask_net(&net, shards_ipc::kind::NET_NAMES, &table, &[]);
            }
        }
    }

    /// MicroVM `id`'s run has ended: no longer a member, its addresses' leases let go, and
    /// its peers' names say so. Its network's pairing goes with its last member, under the
    /// members' lock, where a join finds an entry or pairs (D285): never one paired here with
    /// no entry, which a join would find paired elsewhere.
    pub(super) fn leave_network(&self, id: &str) {
        let left: Vec<String> = {
            let mut members = lock(&self.members);
            let mut left = Vec::new();
            members.retain(|net, joined| {
                let had = joined.members.len();
                joined.members.retain(|e| e.member.container != id);
                if joined.members.len() != had {
                    left.push(net.clone());
                }
                !joined.members.is_empty()
            });
            left
        };
        // A run on no user network: no network to read.
        if left.is_empty() {
            return;
        }
        let networks = self.all_networks();
        for net in left {
            if let Some(n) = networks.iter().find(|n| n.id == net) {
                let _telling = lock(&self.telling);
                self.tell_names(n);
            }
        }
    }

    /// Network `n`'s runs of other processes (D285): each live address lease that no run
    /// of this daemon holds, as a member of its holder's ID and its address alone. None of
    /// a predefined network, which leases none, nor where the home's records cannot be read.
    fn elsewhere(&self, n: &Network) -> Vec<Member> {
        if n.predefined() {
            return Vec::new();
        }
        let ours: Vec<Ipv4Addr> = lock(&self.members)
            .get(&n.id)
            .map(|joined| joined.members.iter().map(|e| e.member.ip).collect())
            .unwrap_or_default();
        let Ok(ledger) = Leases::of(&self.home, &n.id) else {
            return Vec::new();
        };
        let Ok(leased) = leases::hold(&self.home).and_then(|held| ledger.addresses(&held)) else {
            return Vec::new();
        };
        leased
            .into_iter()
            .filter(|(ip, _)| !ours.contains(ip))
            .map(|(ip, holder)| Member {
                container: holder.unwrap_or_default(),
                name: String::new(),
                endpoint: String::new(),
                mac: String::new(),
                ip,
                ip6: None,
                dns_names: Vec::new(),
                link_local: Vec::new(),
            })
            .collect()
    }

    /// `shards network create NAME` (NetworkCreate, Controller.NewNetwork): its flags as
    /// the network's ask, made as `made::create` makes every network, an orchestrator's
    /// among them (D285): the CLI's checks, then dockerd's, each in its order and words; its
    /// ID once kept.
    pub(super) fn network_create(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        let Some(name) = parsed.args.first() else {
            return 1;
        };
        let family = |flag: &str| parsed.changed(flag).then(|| parsed.bool(flag));
        let ask = made::Create {
            name: name.clone(),
            subnets: parsed.many("subnet").to_vec(),
            ranges: parsed.many("ip-range").to_vec(),
            gateways: parsed.many("gateway").to_vec(),
            aux: kv_map(parsed.many("aux-address")),
            driver: parsed.string("driver").to_string(),
            options: kv_map(parsed.many("opt")),
            ipv4: family("ipv4"),
            ipv6: family("ipv6"),
            config_from: parsed.string("config-from").to_string(),
            config_only: parsed.bool("config-only"),
            scope: parsed.string("scope").to_string(),
            ipam_driver: parsed.string("ipam-driver").to_string(),
            ipam_options: kv_map(parsed.many("ipam-opt")),
            internal: parsed.bool("internal"),
            attachable: parsed.bool("attachable"),
            ingress: parsed.bool("ingress"),
            labels: kv_map(parsed.many("label")),
        };
        let n = match made::create(&self.home, self.bridge, &ask) {
            Ok(n) => n,
            // The CLI's own checks are said as it says them; the rest as the daemon's.
            Err((made::Unmade::Flags, why)) => {
                reply.err(&why);
                return 1;
            }
            Err((_, why)) => {
                reply.err(&format!("shards: {why}"));
                return 1;
            }
        };
        reply.out(&n.id);
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

    /// Network `n` in the shape inspect answers (D46, D170): this daemon's runs on it, and
    /// each other process's its leases name (D285).
    pub(super) fn network_value(&self, n: &Network) -> Value {
        let mut members = self.members_of(&n.id);
        members.extend(self.elsewhere(n));
        self.network_value_of(n, &members)
    }

    /// Network `n` as its inspect document says it, with `members` on it: the members of a
    /// network of a microVM's own engine are that engine's joiners (D181).
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

    /// Whether network `n` holds no member (dangling): no run of this daemon's, nor of
    /// another process (D285). Takes the home's networks lock: never asked under it.
    fn idle(&self, n: &Network) -> bool {
        self.members_of(&n.id).is_empty() && self.elsewhere(n).is_empty()
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

    /// The networks `filters` keep, by the filters clients send and in the words scripts
    /// match (D46).
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
    /// each refused said as it comes, then `exit status 1`, as the CLI says them. Each
    /// found and removed under the home's networks lock, let go before it is said (D285).
    pub(super) fn network_rm(&self, parsed: &shards_cmdline::flags::Parsed, reply: &Reply<'_>) -> u8 {
        let mut failed = false;
        for term in &parsed.args {
            let held = match leases::hold(&self.home) {
                Ok(held) => held,
                Err(e) => {
                    reply.err(&format!("shards: {e}"));
                    failed = true;
                    continue;
                }
            };
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
                    } else {
                        made::remove(&self.home, &held, &networks, n).map_err(|(_, why)| why)
                    }
                }
                Found::Ambiguous(count, by_name) => Err(format!(
                    "{term} names {count} networks by {}: give more of its ID",
                    if by_name { "name" } else { "their IDs' beginning" }
                )),
                Found::None if parsed.bool("force") => continue,
                Found::None => Err(format!("no network is named {term}")),
            };
            drop(held);
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
        // Another process's runs keep a network as this daemon's do: its removal refuses it
        // (`made::remove`), under the home's networks lock, which `idle` would take again.
        let held = leases::hold(&self.home).map_err(|e| e.to_string())?;
        let mut names = Vec::new();
        let networks = self.all_networks();
        for n in &networks {
            let keep = n.predefined()
                || n.config_only
                || !self.members_of(&n.id).is_empty()
                || !filters.kv("label", &n.labels)
                || (filters.contains("label!") && filters.kv("label!", &n.labels))
                || until.is_some_and(|u| i128::try_from(n.created).unwrap_or(i128::MAX) > u);
            if keep {
                continue;
            }
            if made::remove(&self.home, &held, &networks, n).is_ok() {
                names.push(n.name.clone());
            }
        }
        drop(held);
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

    /// Each member is answered its own names and those of the members it reaches: all of a
    /// network's whose members reach one another, none but its own on one made with
    /// `enable_icc` false (default deny: no name for a peer no link reaches).
    #[test]
    fn a_member_is_answered_the_names_it_reaches_alone() {
        let member = |id: &str, ip: [u8; 4], names: &[&str]| Member {
            container: id.into(),
            name: String::new(),
            endpoint: String::new(),
            mac: String::new(),
            link_local: Vec::new(),
            ip: Ipv4Addr::from(ip),
            ip6: None,
            dns_names: s(names),
        };
        let members = [
            member("aaa", [10, 0, 0, 2], &["web"]),
            member("bbb", [10, 0, 0, 3], &["db", "pg"]),
        ];
        let mut net = Network {
            name: "app".into(),
            id: "n1".into(),
            created: 0,
            scope: "local".into(),
            driver: "bridge".into(),
            ipv4: true,
            ipv6: false,
            ipam_driver: "default".into(),
            ipam_options: BTreeMap::new(),
            pools: Vec::new(),
            pools6: Vec::new(),
            v6_first: false,
            internal: false,
            attachable: false,
            ingress: false,
            config_from: String::new(),
            config_only: false,
            options: BTreeMap::new(),
            labels: BTreeMap::new(),
        };
        let v4 = |a: [u8; 4]| std::net::IpAddr::V4(Ipv4Addr::from(a));
        let all = shards_net::dns::Names::encode(
            "app",
            &[
                ("web".into(), v4([10, 0, 0, 2])),
                ("db".into(), v4([10, 0, 0, 3])),
                ("pg".into(), v4([10, 0, 0, 3])),
            ],
        );
        assert_eq!(names_table(&net, &members, "aaa"), all);
        assert_eq!(names_table(&net, &members, "bbb"), all);
        net.options.insert(nets::ICC.into(), "false".into());
        assert_eq!(
            names_table(&net, &members, "aaa"),
            shards_net::dns::Names::encode("app", &[("web".into(), v4([10, 0, 0, 2]))])
        );
        assert_eq!(
            names_table(&net, &members, "bbb"),
            shards_net::dns::Names::encode(
                "app",
                &[("db".into(), v4([10, 0, 0, 3])), ("pg".into(), v4([10, 0, 0, 3]))]
            )
        );
    }
}
