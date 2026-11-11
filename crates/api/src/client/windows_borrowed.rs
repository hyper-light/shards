//! ROOT-reviewed source preparation only: exact selectors require an external held-child supervisor.
use super::*;
use shards_afunix::{UnixListener, UnixStream};
use std::cell::RefCell;
use std::os::windows::io::AsRawHandle as _;
use std::process::ChildStdin;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::Networking::WinSock::{
    POLLRDNORM, POLLWRNORM, SOCKET, WSAGetLastError, WSAPOLLFD, WSAPoll,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const PORT: u32 = 1234;
const HELPER: &str = "client::windows_borrowed_contract::held_child";

#[derive(Clone, Copy, Debug)]
struct OriginalClock {
    began: Instant,
    wait: HandshakeWait,
}

enum IdentityGate {
    Observe(Sender<OriginalClock>),
    Expire(Sender<OriginalClock>),
}

thread_local! {
    static IDENTITY_GATE: RefCell<Option<IdentityGate>> = const { RefCell::new(None) };
}

/// No hook is installed in other tests. Neither variant owns a guest, handle or socket.
pub(super) fn before_identity(began: Instant, wait: HandshakeWait) {
    let gate = IDENTITY_GATE.with_borrow_mut(Option::take);
    let clock = OriginalClock { began, wait };
    match gate {
        None => {}
        Some(IdentityGate::Observe(sender)) => sender.send(clock).unwrap(),
        Some(IdentityGate::Expire(sender)) => {
            sender.send(clock).unwrap();
            let HandshakeWait::Until(at) = wait else {
                panic!("positive caller must establish Until before identity");
            };
            until(at);
        }
    }
}

struct Armed;

impl Armed {
    fn new(gate: IdentityGate) -> Self {
        IDENTITY_GATE.with_borrow_mut(|slot| assert!(slot.replace(gate).is_none()));
        Self
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        IDENTITY_GATE.with_borrow_mut(|slot| *slot = None);
    }
}

fn left(at: Instant) -> Duration {
    let remaining = at.saturating_duration_since(Instant::now());
    assert!(!remaining.is_zero(), "original case deadline elapsed");
    remaining
}

fn receive<T>(receiver: &Receiver<T>, at: Instant) -> T {
    receiver.recv_timeout(left(at)).expect("original case deadline")
}

/// A finite silence ending at the supplied absolute instant, without periodic state probes.
fn until(at: Instant) {
    let (_sender, receiver) = mpsc::channel::<()>();
    assert_eq!(
        receiver.recv_timeout(at.saturating_duration_since(Instant::now())),
        Err(RecvTimeoutError::Timeout)
    );
}

fn milliseconds(at: Instant) -> u32 {
    u32::try_from(
        at.saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(1_000_000),
    )
    .unwrap_or(u32::MAX - 1)
}

fn ready(socket: SOCKET, events: i16, at: Instant) {
    let mut entry = WSAPOLLFD {
        fd: socket,
        events,
        revents: 0,
    };
    let timeout = i32::try_from(left(at).as_nanos().div_ceil(1_000_000)).unwrap();
    // SAFETY: one fixture-owned live socket; WSAPoll writes only this entry.
    let returned = unsafe { WSAPoll(&raw mut entry, 1, timeout) };
    let error = if returned < 0 {
        // SAFETY: the same thread's Winsock error immediately after the failed WSAPoll.
        let code = unsafe { WSAGetLastError() };
        Some(io::Error::from_raw_os_error(code))
    } else {
        None
    };
    let _ = writeln!(
        io::stdout(),
        "WB_POLL returned={returned} revents={} error={error:?}",
        entry.revents
    );
    assert!(returned > 0, "fixture readiness failed: {error:?}");
}

fn accept(listener: &UnixListener, at: Instant) -> UnixStream {
    ready(listener.raw(), POLLRDNORM, at);
    let (peer, ()) = listener.accept().unwrap();
    peer.set_nonblocking(true).unwrap();
    peer
}

fn read_request(peer: &mut UnixStream, at: Instant) -> Vec<u8> {
    let expected = format!("CONNECT {PORT}\n");
    let mut bytes = vec![0; expected.len()];
    let mut used = 0;
    while used < bytes.len() {
        ready(peer.raw(), POLLRDNORM, at);
        let n = peer.read(bytes.get_mut(used..).unwrap()).unwrap();
        assert_ne!(n, 0, "EOF before complete CONNECT");
        used = used.checked_add(n).unwrap();
    }
    assert_eq!(bytes, expected.as_bytes());
    let _ = writeln!(io::stdout(), "WB_REQUEST bytes={bytes:?}");
    bytes
}

fn write_bytes(peer: &mut UnixStream, bytes: &[u8], at: Instant) -> io::Result<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        ready(peer.raw(), POLLWRNORM, at);
        let n = peer.write(remaining)?;
        assert_ne!(n, 0, "fixture write made no progress");
        remaining = remaining.get(n..).unwrap();
    }
    Ok(())
}

fn through_eof(peer: &mut UnixStream, at: Instant) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        ready(peer.raw(), POLLRDNORM, at);
        let mut byte = [0];
        let n = peer.read(&mut byte).unwrap();
        if n == 0 {
            let _ = writeln!(io::stdout(), "WB_PEER_EOF bytes={bytes:?}");
            return bytes;
        }
        bytes.push(byte[0]);
        assert!(bytes.len() <= REQUEST, "unexpected extra peer bytes");
    }
}

/// Retains the exact native Child on every setup/panic path. No PID-only kill or Job mutation.
struct ChildOwner {
    process: Arc<Mutex<Child>>,
    control: Option<ChildStdin>,
    deadline: Instant,
}

impl Drop for ChildOwner {
    fn drop(&mut self) {
        drop(self.control.take());
        let mut child = lock(&self.process);
        let before = child.try_wait();
        let kill = if matches!(before, Ok(None)) {
            Some(child.kill())
        } else {
            None
        };
        // SAFETY: Child owns this process handle throughout the finite wait, without closing it.
        let observed = unsafe { WaitForSingleObject(child.as_raw_handle(), milliseconds(self.deadline)) };
        let wait_error = (observed == WAIT_FAILED).then(io::Error::last_os_error);
        let status = if observed == WAIT_OBJECT_0 {
            Some(child.try_wait())
        } else {
            None
        };
        let _ = writeln!(
            io::stdout(),
            "WB_CHILD_DROP before={before:?} kill={kill:?} wait={observed} error={wait_error:?} status={status:?}"
        );
    }
}

struct Fixture {
    guest: Arc<ShardsGuest>,
    held: borrowed_windows::Held,
    child: ChildOwner,
    listener: Arc<UnixListener>,
    directory: PrivateDir,
    deadline: Instant,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let milliseconds: u64 = std::env::var("SHARDS_WINDOWS_BORROWED_MS")
            .unwrap()
            .parse()
            .unwrap();
        assert!(milliseconds > 0);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(milliseconds))
            .unwrap();
        let directory = PrivateDir::new().unwrap();
        let path = directory.0.join("v");
        let listener = Arc::new(UnixListener::bind(&path).unwrap());
        listener.set_nonblocking(true).unwrap();
        let (output, writer) = io::pipe().unwrap();
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([HELPER, "--exact", "--ignored", "--test-threads=1", "--nocapture"])
            .env("SHARDS_WINDOWS_BORROWED_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(writer.try_clone().unwrap())
            .stderr(writer.try_clone().unwrap())
            .spawn()
            .unwrap();
        let control = process.stdin.take().unwrap();
        let child = ChildOwner {
            process: Arc::new(Mutex::new(process)),
            control: Some(control),
            deadline,
        };
        let pid = lock(&child.process).id();
        let started = created(&lock(&child.process)).unwrap();
        let held = borrowed_windows::Held::find(pid, started).unwrap();
        // One authoritative real Shared, initialized as start() does; no second lifecycle ledger.
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                exited: false,
                stopping: false,
                ready: false,
                end: None,
                said: String::new(),
                ended_as: String::new(),
                status: None,
            }),
            changed: Condvar::new(),
            tail: Mutex::new(Tail {
                bytes: VecDeque::new(),
                keep: tail(0),
                drained: false,
                output: None,
            }),
            drained: Condvar::new(),
            end_mark: end_mark(),
        });
        start_threads(&shared, &child.process, output, writer).unwrap();
        let guest = Arc::new(ShardsGuest {
            ports: vec![PORT],
            vsock: path,
            child: child.process.clone(),
            shared,
            _dir: None,
        });
        let fixture = Self {
            guest,
            held,
            child,
            listener,
            directory,
            deadline,
        };
        let _ = writeln!(
            io::stdout(),
            "WB_READY case={name} pid={pid} started={started} socket={}",
            fixture.guest.vsock.display()
        );
        io::stdout().flush().unwrap();
        // ROOT holds/verifies both process identities before releasing this exact case.
        let mut release = [0];
        io::stdin().read_exact(&mut release).unwrap();
        assert_eq!(release, [b'R']);
        left(deadline);
        fixture.alive();
        fixture
    }

    fn identity(&self) -> Identity {
        self.guest.identity().unwrap()
    }

    fn alive(&self) {
        self.held.check().unwrap();
        assert!(lock(&self.child.process).try_wait().unwrap().is_none());
        assert_eq!(self.guest.wait_for(Duration::ZERO).unwrap(), None);
        let _ = writeln!(
            io::stdout(),
            "WB_OWNER_ALIVE pid={} started={}",
            self.identity().pid,
            self.identity().started
        );
    }

    fn no_queued_peer(&self) {
        let error = self.listener.accept().unwrap_err();
        let _ = writeln!(
            io::stdout(),
            "WB_ACCEPT_NONE kind={:?} code={:?}",
            error.kind(),
            error.raw_os_error()
        );
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    fn finish(self) {
        self.guest.stop().unwrap();
        assert_eq!(
            self.guest.wait_for(left(self.deadline)).unwrap(),
            Some(End::Stopped)
        );
        assert_eq!(self.held.check(), Err(crate::ProcessFailure::Exited));
        let status = lock(&self.child.process)
            .try_wait()
            .unwrap()
            .expect("watcher waited exact child");
        let _ = writeln!(
            io::stdout(),
            "WB_OWNER_STOPPED pid={} status={status:?} watch_state_exited=true",
            lock(&self.child.process).id()
        );
        let path = self.directory.0.clone();
        drop(self);
        assert!(!path.exists(), "owned fixture directory remains");
        let _ = writeln!(io::stdout(), "WB_OWNED_DIRECTORY_ABSENT");
    }
}

fn observed(
    id: &Identity,
    patience: Duration,
    expire: bool,
) -> (Result<Box<dyn Stream>, Refusal>, OriginalClock) {
    let (sender, receiver) = mpsc::channel();
    let gate = if expire {
        IdentityGate::Expire(sender)
    } else {
        IdentityGate::Observe(sender)
    };
    let _armed = Armed::new(gate);
    let result = connect_borrowed(id, PORT, patience);
    let clock = receiver.try_recv().expect("one original clock before identity");
    let _ = writeln!(
        io::stdout(),
        "WB_CALL clock={clock:?} success={} refusal={:?}",
        result.is_ok(),
        result.as_ref().err()
    );
    if let HandshakeWait::Until(at) = clock.wait {
        assert_eq!(clock.began.checked_add(patience), Some(at));
    }
    (result, clock)
}

fn read_expired(result: &Result<Box<dyn Stream>, Refusal>) {
    assert!(
        matches!(
            result,
            Err(Refusal::Borrowed {
                port: PORT,
                failure: BorrowFailure::Channel(HandshakeError::Expired {
                    phase: HandshakePhase::Read
                })
            })
        ),
        "expected exact Read expiry, got {:?}",
        result.as_ref().err()
    );
}

#[test]
#[ignore = "support child only; ROOT invokes through the fixture-owned stdin lifetime"]
fn held_child() {
    assert_eq!(std::env::var("SHARDS_WINDOWS_BORROWED_CHILD").unwrap(), "1");
    let mut byte = [0];
    match io::stdin().read(&mut byte).unwrap() {
        0 => {}
        1 => assert_eq!(byte, [b'X']),
        _ => unreachable!("one-byte read"),
    }
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn positive_borrowed_drop_keeps_owner_alive() {
    let fixture = Fixture::new("positive");
    let listener = fixture.listener.clone();
    let at = fixture.deadline;
    let peer = std::thread::spawn(move || {
        let mut peer = accept(&listener, at);
        read_request(&mut peer, at);
        write_bytes(&mut peer, b"OK 7\n\0\xff\n", at).unwrap();
        assert!(through_eof(&mut peer, at).is_empty());
    });
    let (result, _) = observed(&fixture.identity(), crate::conformance::UNSERVED_PATIENCE, false);
    let mut stream = result.unwrap();
    stream.set_read_timeout(Some(left(fixture.deadline))).unwrap();
    let mut bytes = [0; 3];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0, 255, b'\n']);
    drop(stream);
    peer.join().unwrap();
    let _ = writeln!(io::stdout(), "WB_PEER_JOINED positive");
    fixture.alive();
    fixture.no_queued_peer();
    fixture.finish();
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn zero_reports_actual_pending_phase_and_peer_bytes() {
    let fixture = Fixture::new("zero");
    // No accept/reply worker exists until the immediate public call has returned.
    let (result, clock) = observed(&fixture.identity(), Duration::ZERO, false);
    assert!(matches!(clock.wait, HandshakeWait::Ready));
    let phase = match result {
        Err(Refusal::Borrowed {
            port: PORT,
            failure:
                BorrowFailure::Channel(HandshakeError::Io {
                    phase,
                    kind: io::ErrorKind::WouldBlock,
                    code,
                }),
        }) if matches!(
            phase,
            HandshakePhase::Dial | HandshakePhase::Write | HandshakePhase::Read
        ) =>
        {
            let _ = writeln!(io::stdout(), "WB_READY_PENDING phase={phase:?} code={code:?}");
            phase
        }
        other => panic!("missing immediate pending witness: {:?}", other.err()),
    };
    match fixture.listener.accept() {
        Ok((mut peer, ())) => {
            peer.set_nonblocking(true).unwrap();
            let bytes = through_eof(&mut peer, fixture.deadline);
            let expected = format!("CONNECT {PORT}\n");
            assert!(expected.as_bytes().starts_with(&bytes));
            match phase {
                HandshakePhase::Dial => assert!(bytes.is_empty()),
                HandshakePhase::Read => assert_eq!(bytes, expected.as_bytes()),
                HandshakePhase::Write => assert!(bytes.len() < expected.len()),
                _ => unreachable!(),
            }
            let _ = writeln!(
                io::stdout(),
                "WB_ACCEPTED_AFTER_READY phase={phase:?} bytes={bytes:?}"
            );
        }
        Err(error) => {
            let _ = writeln!(
                io::stdout(),
                "WB_READY_NO_ACCEPT phase={phase:?} kind={:?} code={:?}",
                error.kind(),
                error.raw_os_error()
            );
            assert_eq!(phase, HandshakePhase::Dial);
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        }
    }
    fixture.no_queued_peer();
    fixture.alive();
    fixture.finish();
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn max_is_bounds_before_mismatched_identity_or_dial() {
    let fixture = Fixture::new("max");
    let mut id = fixture.identity();
    id.started = id.started.wrapping_add(1);
    let (sender, receiver) = mpsc::channel();
    let _armed = Armed::new(IdentityGate::Observe(sender));
    let result = connect_borrowed(&id, PORT, Duration::MAX);
    let _ = writeln!(io::stdout(), "WB_MAX refusal={:?}", result.as_ref().err());
    assert!(matches!(result, Err(Refusal::Bounds { what: "patience", .. })));
    assert!(matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)));
    fixture.no_queued_peer();
    fixture.alive();
    fixture.finish();
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn positive_deadline_is_established_before_identity() {
    let fixture = Fixture::new("identity-deadline");
    let (result, clock) = observed(&fixture.identity(), crate::conformance::UNSERVED_PATIENCE, true);
    let HandshakeWait::Until(at) = clock.wait else {
        panic!("positive Until")
    };
    assert!(Instant::now() >= at);
    assert!(matches!(
        result,
        Err(Refusal::Borrowed {
            port: PORT,
            failure: BorrowFailure::Channel(HandshakeError::Expired {
                phase: HandshakePhase::Dial
            })
        })
    ));
    fixture.no_queued_peer();
    fixture.alive();
    fixture.finish();
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn withheld_reply_expires_before_peer_release() {
    let fixture = Fixture::new("withheld");
    let listener = fixture.listener.clone();
    let at = fixture.deadline;
    let (requested, request) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let mut peer = accept(&listener, at);
        requested.send(read_request(&mut peer, at)).unwrap();
        receive(&released, at);
        assert!(through_eof(&mut peer, at).is_empty());
    });
    let (result, clock) = observed(&fixture.identity(), crate::conformance::UNSERVED_PATIENCE, false);
    let requested = receive(&request, fixture.deadline);
    let _ = writeln!(
        io::stdout(),
        "WB_WITHHELD_RESULT_BEFORE_RELEASE clock={clock:?} request={requested:?}"
    );
    release.send(()).unwrap();
    peer.join().unwrap();
    read_expired(&result);
    let _ = writeln!(io::stdout(), "WB_PEER_JOINED withheld");
    fixture.alive();
    fixture.no_queued_peer();
    fixture.finish();
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn dribbled_reply_keeps_the_original_deadline() {
    let fixture = Fixture::new("dribbled");
    let listener = fixture.listener.clone();
    let at = fixture.deadline;
    let patience = crate::conformance::UNSERVED_PATIENCE;
    let (sender, receiver) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let clock: OriginalClock = receive(&receiver, at);
        let mut peer = accept(&listener, at);
        read_request(&mut peer, at);
        let mut before_deadline = Vec::new();
        let HandshakeWait::Until(original) = clock.wait else {
            panic!("positive Until")
        };
        for (index, byte) in b"OK 7\n".iter().enumerate() {
            let tick = clock
                .began
                .checked_add((patience / 2).checked_mul(u32::try_from(index).unwrap()).unwrap())
                .unwrap();
            until(tick);
            let written = write_bytes(&mut peer, &[*byte], at);
            let _ = writeln!(
                io::stdout(),
                "WB_DRIP byte={byte} elapsed={:?} result={written:?}",
                clock.began.elapsed()
            );
            if written.is_err() {
                break;
            }
            if Instant::now() < original {
                before_deadline.push(*byte);
            }
        }
        assert!(
            !before_deadline.is_empty(),
            "no actual pre-expiry peer byte; coverage missing"
        );
        let _ = writeln!(io::stdout(), "WB_DRIP_BEFORE_DEADLINE bytes={before_deadline:?}");
    });
    let armed = Armed::new(IdentityGate::Observe(sender));
    let result = connect_borrowed(&fixture.identity(), PORT, patience);
    drop(armed);
    let _ = writeln!(io::stdout(), "WB_DRIP_REFUSAL {:?}", result.as_ref().err());
    peer.join().unwrap();
    read_expired(&result);
    let _ = writeln!(io::stdout(), "WB_PEER_JOINED dribbled");
    fixture.alive();
    fixture.no_queued_peer();
    fixture.finish();
}

fn owning_waits_for_answer(name: &str, patience: Duration) {
    let fixture = Fixture::new(name);
    let listener = fixture.listener.clone();
    let at = fixture.deadline;
    let answer_at = Instant::now()
        .checked_add(crate::conformance::UNSERVED_PATIENCE)
        .unwrap();
    let peer = std::thread::spawn(move || {
        let mut peer = accept(&listener, at);
        read_request(&mut peer, at);
        assert!(
            Instant::now() < answer_at,
            "fixture missed original answer-release instant"
        );
        until(answer_at);
        write_bytes(&mut peer, b"OK 7\n", at).unwrap();
        assert!(through_eof(&mut peer, at).is_empty());
    });
    let stream = fixture.guest.connect(PORT, patience).unwrap();
    assert!(Instant::now() >= answer_at);
    drop(stream);
    peer.join().unwrap();
    let _ = writeln!(
        io::stdout(),
        "WB_OWNING_CONNECTED patience={patience:?} peer_joined=true"
    );
    fixture.alive();
    fixture.no_queued_peer();
    fixture.finish();
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn owning_zero_still_awaits_one_answer() {
    owning_waits_for_answer("owning-zero", Duration::ZERO);
}

#[test]
#[ignore = "ROOT-reviewed native Windows case; held supervisor and original caller deadline required"]
fn owning_max_still_has_no_clock() {
    owning_waits_for_answer("owning-max", Duration::MAX);
}
