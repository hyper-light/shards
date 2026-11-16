//! The runs of this process linked on networks shared by runs (D285 slice 4). This process,
//! the orchestrator's, holds the channel of every run host it started, so it links each two of
//! its runs on a network: it makes their link, a stream pair as D46's peers' (the daemon's
//! `NET_PEER`), and hands each run's host its end, which hands it to its network process
//! (`NET_LINK`): on Unix on the host's links socket (`wire::LINKS_FD`), as its pipes carry no
//! socket; on Windows within the ask, shared with the host's process (WSADuplicateSocketW, PM
//! M203). Its run hosts hold each network's pairing under this process's token
//! (`Leases::pair_as`), so that no other process's runs are linked on it meanwhile.
//!
//! One change of a shared network at a time in this process ([`TURN`]): a member's grant made
//! by its run's host, then each other run of this process's on the network told its members
//! again (`Rejoin`), and, where the run is new to the network, each linked to it, its own
//! members having been told theirs as it joined. A run that has ended is let go of at its
//! network's next change: its link went with its network process, so nothing reaches it
//! meanwhile.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::super::{ImageRefusal, wire};
use super::{Shared, lock};

/// A link's end, as a run's host is handed it: a Unix stream socket; on Windows an AF_UNIX
/// socket, which network processes wait on with their own (PM M213).
#[cfg(unix)]
type End = std::os::unix::net::UnixStream;
#[cfg(windows)]
type End = shards_afunix::UnixStream;

/// A run of this process's on a shared network: its channel, its host's process ID (with
/// which a link's socket is shared on Windows), and its address there.
#[derive(Debug, Clone)]
struct Run {
    shared: Weak<Shared>,
    host: u32,
    address: Ipv4Addr,
}

/// This process's runs on a shared network, by the network's ID.
#[derive(Debug)]
struct Linked {
    id: String,
    runs: Vec<Run>,
}

/// Every shared network this process links runs on.
#[derive(Debug)]
struct Linking(Vec<Linked>);

static ON: Mutex<Linking> = Mutex::new(Linking(Vec::new()));

/// The turn of a change of a shared network: held from its run's ask to its last link, so
/// that no two changes interleave their links and members' tellings.
static TURN: Mutex<()> = Mutex::new(());

/// This process's token, under which its run hosts hold each network's pairing: 64 lowercase
/// hex digits, a microVM ID's form, as a lease's holder is, so that it names no path; 256 bits
/// of std's `RandomState`, seeded from the OS's random source, which no other orchestrator of
/// the home shares (as `crate::client::private_name`'s nonce).
fn token() -> &'static str {
    use std::hash::BuildHasher as _;
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        (0u8..4)
            .map(|i| {
                let word = std::collections::hash_map::RandomState::new().hash_one((std::process::id(), i));
                format!("{word:016x}")
            })
            .collect()
    })
}

/// Member `member` of the run `shared` (its host's process `host`) given `grant`, whole and
/// checked, on shared network `network`: asked of its host; then each other run of this
/// process's on the network told its members again, and, where the run is new to it, linked
/// to each. The run's places on shared networks as its host answered them.
pub(super) fn connect(
    shared: &Arc<Shared>,
    host: u32,
    member: u32,
    network: &str,
    grant: &wire::NetworkGrant,
    patience: Duration,
) -> Result<Vec<wire::OnNetwork>, ImageRefusal> {
    let _turn = lock(&TURN);
    let change = wire::NetChange::Connect {
        member,
        network: network.to_string(),
        grant: grant.clone(),
        pairer: token().to_string(),
    };
    let changed = shared.net(change, patience)?;
    let on = changed
        .networks
        .iter()
        .find(|n| n.network == network || n.id == network)
        .ok_or_else(|| {
            ImageRefusal::Host(format!(
                "it answered a connection to {network} with the run on no such network"
            ))
        })?;
    let others = others(shared, &on.id);
    for (other, _) in &others {
        told_again(other, &on.id, patience);
    }
    if !registered(shared, &on.id) {
        for (other, at) in &others {
            link(shared, host, on.address, other, at, &on.id, patience)?;
        }
        lock(&ON).register(shared, host, &on.id, on.address);
    }
    Ok(changed.networks)
}

/// `grant` of member `member` of the run `shared` taken back on shared network `network`:
/// asked of its host; the run let go of where it left the network with it; each other run of
/// this process's there told its members again.
pub(super) fn disconnect(
    shared: &Arc<Shared>,
    member: u32,
    network: &str,
    grant: &wire::NetworkGrant,
    patience: Duration,
) -> Result<Vec<wire::OnNetwork>, ImageRefusal> {
    let _turn = lock(&TURN);
    let id = lock(&shared.state)
        .networks
        .iter()
        .find(|n| n.network == network || n.id == network)
        .map(|n| n.id.clone());
    let change = wire::NetChange::Disconnect {
        member,
        network: network.to_string(),
        grant: grant.clone(),
    };
    let changed = shared.net(change, patience)?;
    if let Some(id) = id {
        if !changed.networks.iter().any(|n| n.id == id) {
            lock(&ON).unregister(shared, &id);
        }
        for (other, _) in others(shared, &id) {
            told_again(&other, &id, patience);
        }
    }
    Ok(changed.networks)
}

impl Linking {
    /// `shared`'s run on network `id` at `address`.
    fn register(&mut self, shared: &Arc<Shared>, host: u32, id: &str, address: Ipv4Addr) {
        let run = Run {
            shared: Arc::downgrade(shared),
            host,
            address,
        };
        match self.0.iter_mut().find(|l| l.id == id) {
            Some(linked) => linked.runs.push(run),
            None => self.0.push(Linked {
                id: id.to_string(),
                runs: vec![run],
            }),
        }
    }

    /// `shared`'s run off network `id`; the network forgotten with its last run.
    fn unregister(&mut self, shared: &Arc<Shared>, id: &str) {
        for linked in self.0.iter_mut().filter(|l| l.id == id) {
            linked
                .runs
                .retain(|r| !std::ptr::eq(r.shared.as_ptr(), Arc::as_ptr(shared)));
        }
        self.0.retain(|l| !l.runs.is_empty());
    }
}

/// Whether `shared`'s run is linked on network `id`.
fn registered(shared: &Arc<Shared>, id: &str) -> bool {
    lock(&ON).0.iter().any(|l| {
        l.id == id
            && l.runs
                .iter()
                .any(|r| std::ptr::eq(r.shared.as_ptr(), Arc::as_ptr(shared)))
    })
}

/// The other runs of this process's on network `id`, each held while it is asked; each that
/// has ended let go of first.
fn others(shared: &Arc<Shared>, id: &str) -> Vec<(Arc<Shared>, Run)> {
    let mut on = lock(&ON);
    for linked in on.0.iter_mut().filter(|l| l.id == id) {
        linked
            .runs
            .retain(|r| r.shared.upgrade().is_some_and(|s| !ended(&s)));
    }
    on.0.retain(|l| !l.runs.is_empty());
    on.0.iter()
        .filter(|l| l.id == id)
        .flat_map(|l| l.runs.iter())
        .filter_map(|r| {
            let other = r.shared.upgrade()?;
            (!Arc::ptr_eq(&other, shared)).then(|| (other, r.clone()))
        })
        .collect()
}

/// Whether a run's host has said its end, or is gone.
fn ended(s: &Shared) -> bool {
    let state = lock(&s.state);
    state.end.is_some() || state.gone()
}

/// Run `other` told network `id`'s members again; one whose host has ended let go of, as no
/// link reaches it.
fn told_again(other: &Arc<Shared>, id: &str, patience: Duration) {
    let change = wire::NetChange::Rejoin {
        network: id.to_string(),
    };
    if let Err(ImageRefusal::Ended(_) | ImageRefusal::Host(_)) = other.net(change, patience) {
        lock(&ON).unregister(other, id);
    }
}

/// The run `shared` (its host `host`, at `address`) and run `other` (at `at`) linked on network
/// `id`: a stream pair made, each end handed to its run's host, each told where its peer is,
/// the other's first. One the other's host cannot take is that run's to lose alone: let go of
/// where its host has ended.
fn link(
    shared: &Arc<Shared>,
    host: u32,
    address: Ipv4Addr,
    other: &Arc<Shared>,
    at: &Run,
    id: &str,
    patience: Duration,
) -> Result<(), ImageRefusal> {
    let (ours, theirs) = End::pair().map_err(|e| ImageRefusal::Os {
        what: "a shared network's link",
        kind: e.kind(),
    })?;
    match hand(other, at.host, id, address, theirs, patience) {
        Ok(()) => {}
        Err(ImageRefusal::Ended(_) | ImageRefusal::Host(_)) => {
            lock(&ON).unregister(other, id);
            return Ok(());
        }
        Err(e) => return Err(e),
    }
    hand(shared, host, id, at.address, ours, patience)
}

/// `end`, a link on network `id` to the run at `peer`, handed to the run `shared`'s host (its
/// process `host`) and its ask made; `end` held until it is answered, as a socket in flight
/// that no process holds is lost (M24).
fn hand(
    shared: &Shared,
    host: u32,
    id: &str,
    peer: Ipv4Addr,
    end: End,
    patience: Duration,
) -> Result<(), ImageRefusal> {
    let os = |e: std::io::Error| ImageRefusal::Os {
        what: "a shared network's link",
        kind: e.kind(),
    };
    #[cfg(unix)]
    let socket = {
        use std::os::fd::AsFd as _;
        let _ = host;
        let links = shared
            .links
            .as_ref()
            .ok_or_else(|| ImageRefusal::Host("it holds no links socket".into()))?;
        // As `NET_LINK` carries a link (shards_net::shared::encode_link): the network's ID,
        // then the peer's address; the socket beside.
        let payload = [id.as_bytes(), peer.octets().as_slice()].concat();
        shards_ipc::send(links, shards_ipc::kind::NET_LINK, &payload, &[end.as_fd()]).map_err(os)?;
        Vec::new()
    };
    #[cfg(windows)]
    let socket = {
        let raw = u64::try_from(end.raw()).map_err(|_| ImageRefusal::Os {
            what: "a shared network's link",
            kind: std::io::ErrorKind::InvalidData,
        })?;
        shards_ipc::share_socket(raw, host).map_err(os)?
    };
    let change = wire::NetChange::Link {
        network: id.to_string(),
        peer,
        socket,
    };
    let answered = shared.net(change, patience).map(drop);
    drop(end);
    answered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's shared state with no host behind it.
    fn run() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(super::super::State::default()),
            changed: std::sync::Condvar::new(),
            to_host: Mutex::new(None),
            lessor: Mutex::new(None),
            granted: Mutex::new(Vec::new()),
            sign_ins: Mutex::new(Vec::new()),
            said_to: Mutex::new(None),
            timed: false,
            #[cfg(unix)]
            links: None,
        })
    }

    /// This process's token is a microVM ID's form, as a lease's holder is, and the same for
    /// each of its connections.
    #[test]
    fn this_processs_token_is_an_ids_form_and_its_own() {
        let t = token();
        assert_eq!(t.len(), 64);
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(token(), t);
    }

    /// Do register two runs on a network, look for each one's others, have one's host say its
    /// end, look again, then unregister the other. Expect: each run linked there, the other its
    /// one other with its host and address; the ended run let go of at the next look; the
    /// network forgotten with its last run.
    #[test]
    fn a_networks_runs_are_linked_until_they_leave_or_end() {
        let (a, b) = (run(), run());
        let id = "f".repeat(64);
        let (at_a, at_b) = (Ipv4Addr::new(10, 89, 0, 2), Ipv4Addr::new(10, 89, 0, 3));
        lock(&ON).register(&a, 1, &id, at_a);
        lock(&ON).register(&b, 2, &id, at_b);
        assert!(registered(&a, &id) && registered(&b, &id));
        let of_a = others(&a, &id);
        assert_eq!(of_a.len(), 1);
        assert!(Arc::ptr_eq(&of_a[0].0, &b));
        assert_eq!((of_a[0].1.host, of_a[0].1.address), (2, at_b));
        drop(of_a);
        lock(&b.state).end = Some(wire::RunEnded::unused(
            wire::RunEnd::Stopped,
            wire::Unused::Killed,
        ));
        assert!(others(&a, &id).is_empty());
        assert!(!registered(&b, &id), "an ended run let go of");
        lock(&ON).unregister(&a, &id);
        assert!(
            !lock(&ON).0.iter().any(|l| l.id == id),
            "the network forgotten with its last run"
        );
    }
}
