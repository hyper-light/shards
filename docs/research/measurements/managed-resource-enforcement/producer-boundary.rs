//! Public producer boundary, using the canonical real QUIC observer/ACK path.

#[path = @OBSERVER_PATH@]
mod observer;

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use observer::{Ask, Event, Metadata, Observer};
use shards_abi::run::managed::Channel;
use shards_remote::identity::Identity;
use shards_remote::managed::{self, Admitted, Control, Endpoint, Error, Producer, RunId, Scope};
use shards_remote::net::Command;

// Accept both pre-guard and checked signatures without changing the observed
// public API result; the original Option is exactly an Ok of that Option.
trait LoanResult {
    fn result(self) -> Result<Option<Vec<u8>>, Error>;
}
impl LoanResult for Option<Vec<u8>> {
    fn result(self) -> Result<Option<Vec<u8>>, Error> { Ok(self) }
}
impl LoanResult for Result<Option<Vec<u8>>, Error> {
    fn result(self) -> Result<Option<Vec<u8>>, Error> { self }
}

struct Running {
    control: Control,
    observer: Observer,
    ended: mpsc::Receiver<Result<(), Error>>,
    thread: Option<JoinHandle<()>>,
    until: Instant,
}

impl Running {
    fn finish(&mut self) -> bool {
        self.control.abort();
        let _ = self.observer.sender.send(Command::Handler(Ask::Cancel));
        let client = self.observer.join(self.until).is_ok();
        let Some(thread) = self.thread.take() else { return client };
        let server = self.ended.recv_timeout(self.until.saturating_duration_since(Instant::now()));
        match server {
            Ok(result) => result.is_ok() && thread.join().is_ok() && client,
            Err(_) => false,
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) { let _ = self.finish(); }
}

fn ready(producer: &Producer, changes: &mpsc::Receiver<()>, until: Instant) -> usize {
    loop {
        let capacity = producer.capacity();
        if capacity != 0 { return capacity; }
        changes.recv_timeout(until.saturating_duration_since(Instant::now())).unwrap();
    }
}

#[test]
fn a_small_owned_submission_cannot_lend_less_storage_than_its_next_credit() {
    let millis: u64 = std::env::var("SHARDS_CAPTURE_API_CASE_MS").unwrap().parse().unwrap();
    assert!(millis > 0);
    let until = Instant::now().checked_add(Duration::from_millis(millis)).unwrap();
    let owner = Identity::generate().unwrap();
    let scope = Scope { run: RunId([0; 32]), generation: 1, member: 0, channel: Channel::Stdout };
    let capacity = managed::CONTROL_BYTES;
    let (endpoint, producer, control) = Endpoint::bind(
        &owner, owner.fingerprint(), scope,
        Admitted {
            record_bytes: capacity,
            transport_bytes: u64::try_from(capacity + managed::DATA_HEAD).unwrap(),
            // RFC 8446's uint24 handshake length and four-byte header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
            patience: until.saturating_duration_since(Instant::now()),
        },
    ).unwrap();
    let metadata = Metadata {
        server: endpoint.fingerprint, owner: owner.fingerprint(), run: "controlled-boundary".into(),
        generation: scope.generation, capacity, endpoints: BTreeMap::new(),
    };
    let address = endpoint.address;
    let (finished, ended) = mpsc::channel();
    let thread = std::thread::Builder::new().name("capture-api-boundary".into()).spawn(move || {
        let _ = finished.send(endpoint.run());
    }).unwrap();
    let observer = Observer::start(&owner, &metadata, scope, address, until);
    let mut running = Running { control, observer, ended, thread: Some(thread), until };
    let (changed, changes) = mpsc::sync_channel(1);
    producer.on_credit(move || { let _ = changed.try_send(()); });
    assert_eq!(ready(&producer, &changes, until), capacity);
    producer.submit(vec![b'x']).unwrap();
    match running.observer.next(until) {
        Event::Record(0, bytes) => assert_eq!(bytes, b"x"),
        other => panic!("first public submit did not cross QUIC: {other:?}"),
    }
    running.observer.sender.send(Command::Handler(Ask::Ack(1))).unwrap();
    let advertised = ready(&producer, &changes, until);
    let valid = match producer.take_buffer().result() {
        Ok(Some(bytes)) => {
            let actual = bytes.capacity();
            println!("CAPTURE_API_BOUNDARY event=loan advertised={advertised} actual={actual} acknowledged=1");
            running.control.abort();
            drop(bytes);
            actual >= advertised
        }
        Err(Error::Admission(managed::AdmissionError::Capacity)) => {
            println!("CAPTURE_API_BOUNDARY event=typed-refusal advertised={advertised} acknowledged=1");
            // Refusing a loan must preserve the existing owned-Vec submit API.
            let bytes = vec![b'y'; advertised];
            producer.submit(bytes).unwrap();
            match running.observer.next(until) {
                Event::Record(1, bytes) => assert_eq!(bytes, vec![b'y'; advertised]),
                other => panic!("owned submission after refusal did not cross QUIC: {other:?}"),
            }
            true
        }
        other => panic!("unexpected public loan result: {other:?}"),
    };
    let cleaned = running.finish();
    println!("CAPTURE_API_BOUNDARY event=cleanup joined={cleaned}");
    assert!(cleaned, "observer/server must join before the decisive result");
    assert!(valid, "public producer loan is smaller than its advertised capacity");
}
