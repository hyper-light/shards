//! Both arms' protocol paths on this host's loopback, no microVM: arm g's endpoint as its guest
//! runs it, and arm h's endpoint relaying to a stand-in for a VM's vsock socket that answers
//! Firecracker's `CONNECT` as the VMM's muxer does and serves each stream as arm h's guest does.
//! What the real microVMs add is the run's to measure (README.md).

use std::time::Duration;

use crate::client::{Ask, Said};
use crate::run::Clients;

const WAIT: Duration = Duration::from_secs(30);

/// Connects, then 3 round trips, an upload and a download of 1 MiB, and the guest's counts,
/// asserting each answer.
fn exercise(clients: &Clients, arm: &str) {
    clients.ask(0..1, Ask::Connect);
    let said = clients.collect(1, WAIT);
    assert!(matches!(said.as_slice(), [Said::Connected { vm: 0, .. }]), "{said:?}");
    clients.ask(0..1, Ask::Ping { rounds: 3, size: 64 });
    let said = clients.collect(1, WAIT);
    let [Said::Pinged { rtt_ns, .. }] = said.as_slice() else {
        panic!("{said:?}");
    };
    assert_eq!(rtt_ns.len(), 3);
    for ask in [Ask::Upload(1 << 20), Ask::Download(1 << 20)] {
        clients.ask(0..1, ask);
        let said = clients.collect(1, WAIT);
        assert!(
            matches!(said.as_slice(), [Said::Moved { bytes, .. }] if *bytes == 1 << 20),
            "{said:?}"
        );
    }
    clients.ask(0..1, Ask::Stats);
    let said = clients.collect(1, WAIT);
    let [Said::Stats { line, .. }] = said.as_slice() else {
        panic!("{said:?}");
    };
    assert!(line.contains(&format!("arm={arm}")), "{line}");
}

#[test]
fn arm_g_serves_round_trips_transfers_and_counts_over_quic() {
    let controller = shards_remote::identity::Identity::generate().unwrap();
    let (mut driver, fingerprint) =
        crate::guest::bind_quic("127.0.0.1:0".parse().unwrap(), controller.fingerprint()).unwrap();
    let at = driver.local().unwrap();
    std::thread::spawn(move || driver.run());
    let clients = Clients::start(&[(at, fingerprint)], &controller, 64).unwrap();
    exercise(&clients, "g");
    clients.close();
}

/// A controller of another key reaches nothing: its connection is closed as it is made.
#[test]
fn arm_g_refuses_another_controller() {
    let controller = shards_remote::identity::Identity::generate().unwrap();
    let stranger = shards_remote::identity::Identity::generate().unwrap();
    let (mut driver, fingerprint) =
        crate::guest::bind_quic("127.0.0.1:0".parse().unwrap(), controller.fingerprint()).unwrap();
    let at = driver.local().unwrap();
    std::thread::spawn(move || driver.run());
    let clients = Clients::start(&[(at, fingerprint)], &stranger, 64).unwrap();
    clients.ask(0..1, Ask::Connect);
    let said = clients.collect(1, WAIT);
    assert!(matches!(said.as_slice(), [Said::Failed { .. }]), "{said:?}");
    clients.close();
}

/// A VM's vsock socket as the VMM's muxer serves it to the host: `CONNECT <port>` answered
/// `OK <host port>`, then the guest's side of the stream, as arm h's guest serves it.
#[cfg(unix)]
fn fake_vm(sock: &std::path::Path) {
    use std::io::{BufRead as _, Read as _, Write as _};
    let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            std::thread::spawn(move || {
                let mut line = String::new();
                std::io::BufReader::new(&mut stream).read_line(&mut line).unwrap();
                assert_eq!(line, format!("CONNECT {}\n", crate::proto::VSOCK_PORT));
                stream.write_all(b"OK 1073741824\n").unwrap();
                let answered = crate::guest::answer(&mut stream, "arm=h");
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
                answered.unwrap();
            });
        }
    });
}

#[cfg(unix)]
#[test]
fn arm_h_relays_round_trips_transfers_and_counts_to_the_guest() {
    let dir = shards_testdir::TempDir::new("ce-h").unwrap();
    let sock = dir.path().join(crate::endpoint::sock_name(0));
    fake_vm(&sock);
    let controller = shards_remote::identity::Identity::generate().unwrap();
    let (mut driver, at, fingerprint) =
        crate::endpoint::bind("127.0.0.1:0".parse().unwrap(), controller.fingerprint(), vec![sock]).unwrap();
    std::thread::spawn(move || driver.run());
    let clients = Clients::start(&[(at, fingerprint)], &controller, 64).unwrap();
    exercise(&clients, "h");
    clients.close();
}
