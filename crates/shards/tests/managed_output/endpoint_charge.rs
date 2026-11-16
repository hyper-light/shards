//! D252's charge of a daemon's run's agent-control endpoint on a real microVM, its daemon told
//! what its host has available (D188's told pipe, `common::Told`), as no test may set its
//! host's own. Written for ROOT to run (real-VM suites are the coordinator's):
//! `cargo test -p shards --profile test-release --test managed_output endpoint_charge`.
use super::log_lanes::Home;
use super::*;

use shards_remote::managed::{DATAGRAM_BYTES, Endpoint, RunId};

/// What a run refused as it started asked of its daemon's budget, and what that budget had
/// left of what its host has available: the daemon's words for the charge it refused
/// (daemon/managed.rs `Admission::Insufficient`).
#[track_caller]
fn refused(run: &common::Run) -> (u64, u64) {
    assert_ne!(run.status, Some(0), "started with no room: {run}");
    let said = format!("{}{}", run.stdout, run.stderr);
    said.split_once("managed output admission: ")
        .and_then(|(_, rest)| rest.split_once(" bytes requested, "))
        .and_then(|(requested, rest)| {
            let (left, _) = rest.split_once(" available")?;
            Some((requested.parse().ok()?, left.parse().ok()?))
        })
        .unwrap_or_else(|| panic!("refused, but not for its room: {run}"))
}

/// Tells `home`'s daemon its host has `bytes` of memory available, and waits until its log says
/// it heard it, judged as the tests' waits are: every charge it makes from then on is held to
/// it.
#[track_caller]
fn tell(home: &Home, told: &mut common::Told, bytes: u64) {
    told.available(bytes);
    let log = home.home.join("daemon.log");
    let said = format!("told the host has {bytes} bytes of memory available");
    let mut patience = common::Patience::of(&home.home);
    while !std::fs::read_to_string(&log).unwrap_or_default().contains(&said) {
        assert!(patience.holds(), "its daemon never said it heard {bytes} bytes");
        std::thread::sleep(patience.next_look());
    }
    patience.done();
}

/// The kernel's queues of an endpoint's three sockets on this host, each as its OS reports it
/// (`SO_SNDBUF` and `SO_RCVBUF`): those of an endpoint of one stream bound here (step 5's),
/// whose driver holds two, its served socket and its wake (`Endpoint::kernel_buffer_bytes`),
/// and whose control its waker (`Control::kernel_buffer_bytes`), each a loopback UDP socket
/// bound by the driver an agent-control endpoint binds with (`net::Driver::bind_bounded`).
fn three_sockets_queues() -> u64 {
    let identity = Identity::generate().unwrap();
    let scope = Scope {
        run: RunId([0; 32]),
        generation: 0,
        member: 0,
        channel: Channel::Stdout,
    };
    let admitted = shards_api::control::exec_admissions().output;
    let (endpoint, _producer, control) =
        Endpoint::bind(&identity, identity.fingerprint(), scope, admitted).unwrap();
    endpoint.kernel_buffer_bytes().unwrap() + control.kernel_buffer_bytes().unwrap()
}

/// D252's endpoint charge, once whatever the endpoint serves (its amendment "a first
/// generation charged before its members start, and the endpoint's sockets' queues
/// charged"): a daemon's agent-control run is charged its endpoint's link, a TLS handshake's
/// crypto per connection and a datagram each way, and the kernel's queues of its three
/// sockets, its driver's, its wake's and its waker's, each once, whether it owns an agent's
/// output or none. Its daemon, a microVM running so that it listens (D188), told its host has:
/// - nothing: a run is refused at its reservation, which it says;
/// - that reservation: a run owning agent `a`'s output, and one owning none, are refused at
///   their endpoint, each saying the endpoint's charge: the link and the three sockets' queues
///   of an endpoint bound here. A waker charged twice, or not at all where no output is owned
///   (as before lane charge-2), is one socket's queues more or less;
/// - the most a u64 counts: the run starts.
#[test]
fn a_daemon_runs_agent_control_endpoint_is_charged_its_waker_once_whatever_it_owns() {
    Home::with_told("charged", |home, told| {
        home.image("charged:1", &["sleep"], &["sleep"]);
        let (image, _) = common::served();
        let kept = home.command(&["run", "vm", "-d", "--name", "kept", &image, "sleep"]);
        assert_eq!(kept.status, Some(0), "{kept}");
        let start = |owned: &[&str]| {
            let mut args = vec!["run", "vm", "-d", "--annotation", "shards.control=true"];
            args.extend_from_slice(owned);
            args.extend(["charged:1", "sleep"]);
            home.command(&args)
        };
        let owning = ["--annotation", "shards.output=agent:a"];
        tell(home, told, 0);
        let (reservation, left) = refused(&start(&owning));
        assert_eq!(left, 0, "its reservation refused with nothing available");
        tell(home, told, reservation);
        // A handshake's crypto for each connection it holds at once, and a datagram each way
        // (daemon/managed.rs `link_bytes`: RFC 8446 §4's largest handshake message, D238's
        // connections, D180's datagram).
        let link =
            shards_api::control::HANDSHAKE_BYTES * shards_remote::control::CONNECTIONS + 2 * DATAGRAM_BYTES;
        let once = u64::try_from(link).unwrap() + three_sockets_queues();
        assert_eq!(refused(&start(&owning)), (once, 0), "owning agent a's output");
        assert_eq!(refused(&start(&[])), (once, 0), "owning no output");
        tell(home, told, u64::MAX);
        let started = start(&owning);
        assert_eq!(started.status, Some(0), "{started}");
    });
}
