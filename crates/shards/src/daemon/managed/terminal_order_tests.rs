//! ROOT-run fixtures at the real Output/Life/Control/Shared/File seam.
//!
//! These streams are empty and unsubscribed. Finished(0) comes from the real
//! Control::finish path, not a manufactured application ACK or network receipt.
//! No actor, credit worker, admission measurement or VM is represented here.

#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    fn supplied() -> Self {
        let millis: u64 = std::env::var("SHARDS_TERMINAL_ORDER_CASE_MS")
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(millis, 0, "finite caller deadline required");
        Self(Instant::now().checked_add(Duration::from_millis(millis)).unwrap())
    }

    fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    fn receive<T>(self, receiver: &Receiver<T>) -> T {
        receiver.recv_timeout(self.remaining()).unwrap()
    }
}

enum Metadata {
    Writable,
    ReadOnly,
}

struct Fixture {
    output: Arc<Output>,
    producer: Producer,
    credits: Arc<Credits>,
    path: PathBuf,
    deadline: Deadline,
    driver: Option<JoinHandle<()>>,
    returned: Receiver<Result<(), shards_remote::managed::Error>>,
}

impl Fixture {
    fn new(case: &str, metadata: Metadata) -> Self {
        println!("TERMINAL_ORDER event=main-enter case={case} semantics=empty-unsubscribed");
        let deadline = Deadline::supplied();
        let dir = PathBuf::from(std::env::var_os("SHARDS_TERMINAL_ORDER_DIRECTORY").unwrap());
        assert!(dir.is_dir(), "caller creates and retains the private directory");
        let path = dir.join("metadata");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        let file = match metadata {
            Metadata::Writable => file,
            Metadata::ReadOnly => {
                drop(file);
                File::open(&path).unwrap()
            }
        };
        let identity = identity::Identity::generate().unwrap();
        let scope = Scope {
            run: RunId([0; 32]),
            generation: 1,
            member: 0,
            channel: Channel::Stdout,
        };
        let capacity = shards_remote::managed::CONTROL_BYTES;
        let (endpoint, producer, control) = Endpoint::bind(
            &identity,
            identity.fingerprint(),
            scope,
            Admitted {
                record_bytes: capacity,
                transport_bytes: u64::try_from(capacity + shards_remote::managed::DATA_HEAD).unwrap(),
                handshake_bytes: HANDSHAKE_BYTES,
                datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
            },
        )
        .unwrap();
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots: vec![CreditSlot::Draining],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        let output = Arc::new(Output {
            scope,
            address: endpoint.address,
            control,
            metadata: shared_metadata(file).unwrap(),
            life: Mutex::new(Life::Serving),
            credits: Arc::downgrade(&credits),
            index: 0,
            learning: Mutex::new(Learning {
                quantum: shards_registry::Quantum::new(capacity, capacity).unwrap(),
                sample: None,
                admitted: capacity,
            }),
            // Constructor scaffold only: these tests do not exercise admission,
            // growth or accounting, and observed() has no learning sample.
            charge: Arc::new(Charge {
                budget: Arc::new(Mutex::new(Some(0))),
                bytes: Mutex::new(0),
            }),
        });
        let (sent, returned) = mpsc::sync_channel(1);
        let driver = std::thread::Builder::new()
            .name("terminal-order-endpoint".into())
            .spawn(move || {
                let result = endpoint.run();
                let _ = sent.send(result);
            })
            .unwrap();
        Self {
            output,
            producer,
            credits,
            path,
            deadline,
            driver: Some(driver),
            returned,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap()
    }

    fn expected(&self, life: Life) -> Vec<u8> {
        let mut buffer = [0; TERMINAL_MOST];
        life.record(
            self.output.scope,
            Progress {
                accepted: 0,
                consumed: 0,
            },
            &mut buffer,
        )
        .unwrap()
        .to_vec()
    }

    fn finish(&self, terminal: Terminal) {
        self.output.finish(terminal).unwrap();
        assert_eq!(
            self.output.control.state(),
            State::Finished(Progress {
                accepted: 0,
                consumed: 0
            })
        );
    }

    fn driver_returned(&mut self) {
        let result = self.deadline.receive(&self.returned);
        println!("TERMINAL_ORDER event=driver-return result={result:?}");
        self.driver.take().unwrap().join().unwrap();
        assert!(result.is_ok(), "retain an unexpected real endpoint error");
        assert!(matches!(self.output.control.state(), State::Aborted(_)));
        assert!(matches!(
            lock(&self.credits.state).slots.first(),
            Some(CreditSlot::Revoked)
        ));
    }

    fn pass(&self, case: &str) {
        println!(
            "TERMINAL_ORDER result=pass case={case} bytes={}",
            self.bytes().len()
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // No metadata lock or fallible append is needed to request cleanup.
        // A timeout leaves an explicitly unknown join, bounded by the caller's
        // process deadline; do not turn it into an unbounded destructor join.
        self.output.stop_transport();
        if let Some(driver) = self.driver.take() {
            match self.returned.recv_timeout(self.deadline.remaining()) {
                Ok(result) => {
                    println!("TERMINAL_ORDER event=cleanup-driver-return result={result:?}");
                    println!("TERMINAL_ORDER event=cleanup-join result={:?}", driver.join());
                }
                Err(error) => {
                    println!("TERMINAL_ORDER event=cleanup-join outcome=unknown error={error:?}");
                    drop(driver);
                }
            }
        }
    }
}

fn owner_stop_locked(case: &str, finish_first: Option<Terminal>, expected: Life) {
    let mut fixture = Fixture::new(case, Metadata::Writable);
    if let Some(terminal) = finish_first {
        fixture.finish(terminal);
    }
    let (sent, changed) = mpsc::sync_channel(1);
    let weak = Arc::downgrade(&fixture.output);
    fixture.producer.on_credit(move || {
        if let Some(output) = weak.upgrade() {
            // This is only a bounded receipt of real Shared state. The callback
            // does not mutate Shared, run Life or perform file I/O.
            let _ = sent.try_send(output.control.state());
        }
    });
    let _registration_snapshot = fixture.deadline.receive(&changed);
    let metadata_file = Arc::clone(&fixture.output.metadata);
    let metadata = lock(&metadata_file);
    let output = Arc::clone(&fixture.output);
    let (sent, done) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("terminal-order-stop".into())
        .spawn(move || {
            output.abort();
            let _ = sent.send(());
        })
        .unwrap();
    assert!(matches!(fixture.deadline.receive(&changed), State::Aborted(_)));
    assert_eq!(*lock(&fixture.output.life), expected);
    // Output.abort is still waiting on this actual metadata mutex. Real driver
    // retirement must precede our release of it, regardless of logical outcome.
    fixture.driver_returned();
    assert!(fixture.bytes().is_empty());
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(fixture.output.finish(Terminal::End).is_err());
    drop(metadata);
    fixture.deadline.receive(&done);
    worker.join().unwrap();
    assert_eq!(fixture.bytes(), fixture.expected(expected));
    fixture.output.abort();
    fixture.output.complete();
    assert_eq!(*lock(&fixture.output.life), expected);
    assert_eq!(fixture.bytes(), fixture.expected(expected));
    fixture.pass(case);
}

#[test]
#[ignore = "ROOT native fixture: requires exact caller binary, directory and deadlines"]
fn native_owner_stop_after_finish_while_metadata_locked() {
    owner_stop_locked("owner-finished-lock", Some(Terminal::End), Life::Ended);
}

#[test]
#[ignore = "ROOT native fixture: requires exact caller binary, directory and deadlines"]
fn native_owner_stop_before_finish_while_metadata_locked() {
    owner_stop_locked("owner-serving-lock", None, Life::Aborted(None));
}

#[test]
#[ignore = "ROOT native fixture: pauses the explicit resolve-to-publish seam"]
fn native_completion_wins_then_owner_stops_while_metadata_locked() {
    let case = "completion-lock";
    let mut fixture = Fixture::new(case, Metadata::Writable);
    fixture.finish(Terminal::End);
    let metadata_file = Arc::clone(&fixture.output.metadata);
    let metadata = lock(&metadata_file);
    let output = Arc::clone(&fixture.output);
    let (sent, resolved) = mpsc::sync_channel(1);
    let (finished, done) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("terminal-order-publication".into())
        .spawn(move || {
            // This deliberately splits existing methods, rather than claiming
            // that the unmodified complete() wrapper has an internal test hook.
            let terminal = output.resolve(Resolve::Complete);
            sent.send(terminal).unwrap();
            output.publish(terminal);
            let _ = finished.send(());
        })
        .unwrap();
    assert!(matches!(
        fixture.deadline.receive(&resolved),
        Some((Life::Ended, State::Finished(_)))
    ));
    assert_eq!(*lock(&fixture.output.life), Life::Ended);
    assert!(fixture.bytes().is_empty());
    fixture.output.abort();
    fixture.driver_returned();
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(fixture.bytes().is_empty());
    drop(metadata);
    fixture.deadline.receive(&done);
    worker.join().unwrap();
    assert_eq!(fixture.bytes(), fixture.expected(Life::Ended));
    fixture.output.abort();
    assert_eq!(*lock(&fixture.output.life), Life::Ended);
    assert_eq!(fixture.bytes(), fixture.expected(Life::Ended));
    fixture.pass(case);
}

#[test]
#[ignore = "ROOT native fixture: real read-only File append error"]
fn native_readonly_file_error_keeps_logical_completion_and_retires_driver() {
    let case = "readonly-file";
    let mut fixture = Fixture::new(case, Metadata::ReadOnly);
    fixture.finish(Terminal::End);
    let logged = super::super::LOGGED.load(std::sync::atomic::Ordering::Relaxed);
    fixture.output.complete();
    assert_eq!(*lock(&fixture.output.life), Life::Ended);
    assert!(fixture.bytes().is_empty());
    assert_eq!(
        super::super::LOGGED.load(std::sync::atomic::Ordering::Relaxed),
        logged + 1
    );
    fixture.driver_returned();
    fixture.output.abort();
    fixture.output.complete();
    assert!(fixture.bytes().is_empty());
    assert_eq!(*lock(&fixture.output.life), Life::Ended);
    fixture.pass(case);
}

/// The limit cuts the canonical record's write short: the record is cut back to its last
/// whole line (none, as this fixture's record has no header), so that no channel's line
/// follows a torn one (audit-fix-host-v1 finding 2; this fixture pinned the part that
/// stayed before).
#[cfg(unix)]
#[test]
#[ignore = "ROOT isolated child only: caller sets RLIMIT_FSIZE and ignores SIGXFSZ"]
fn native_file_limit_cuts_back_a_partial_append_and_keeps_known_canonical_errno() {
    let case = "file-limit";
    let mut fixture = Fixture::new(case, Metadata::Writable);
    let prefix: usize = std::env::var("SHARDS_TERMINAL_ORDER_PREFIX_BYTES")
        .unwrap()
        .parse()
        .unwrap();
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only reads this process's limit into a valid local.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) }, 0);
    assert_eq!(limit.rlim_cur, libc::rlim_t::try_from(prefix).unwrap());
    assert_eq!(limit.rlim_max, limit.rlim_cur);
    // Supplied canonical capture cause, not an observed guest capture failure.
    let terminal = Terminal::Aborted(libc::EIO);
    let expected_life = Life::Aborted(Some(terminal));
    let expected = fixture.expected(expected_life);
    assert!(
        prefix > 0 && prefix < expected.len(),
        "caller must select a strict nonempty prefix"
    );
    fixture.finish(terminal);
    let logged = super::super::LOGGED.load(std::sync::atomic::Ordering::Relaxed);
    fixture.output.complete();
    assert_eq!(*lock(&fixture.output.life), expected_life);
    assert!(fixture.bytes().is_empty());
    assert_eq!(lock(&fixture.output.metadata).whole, Whole::To(0));
    assert_eq!(
        super::super::LOGGED.load(std::sync::atomic::Ordering::Relaxed),
        logged + 1
    );
    fixture.driver_returned();
    fixture.output.abort();
    fixture.output.complete();
    assert_eq!(*lock(&fixture.output.life), expected_life);
    assert!(fixture.bytes().is_empty());
    // A separate actual File probe, of a file beside the record, records the limit's
    // errno past the prefix (the original publish error is retained by the daemon log
    // above).
    let probe = fixture.path.with_file_name("limit-probe");
    let error = File::create(&probe).unwrap().write_all(&expected).unwrap_err();
    println!(
        "TERMINAL_ORDER event=independent-file-error-probe errno={:?}",
        error.raw_os_error()
    );
    assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
    assert!(fixture.bytes().is_empty());
    fixture.pass(case);
}
