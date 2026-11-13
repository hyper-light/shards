//! A daemon and its clients over QUIC on this machine's loopback (D180): a client joins by
//! an invitation, then runs commands, their output and status crossing whole; a client not
//! enrolled, and a join with another secret, are refused in the daemon's words. And what a
//! stream's ends may do to it: a daemon end it with its last frames at once, a command leave
//! its stdin unread, a client send none, leave the stream, or go, none of which holds up,
//! keeps or reaches another command. Its machine's commands are the tests' helper
//! program's (`shards_testdir::helper`), the same on every host.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use hyper_quic::StreamId;
use shards_remote::Error;
use shards_remote::client::{self, Controls, Remote};
use shards_remote::frame::{self, Request, Tag};
use shards_remote::identity::Identity;
use shards_remote::invite::{self, Invitation};
use shards_remote::net::{self, Close, ConnKey, Driver, Handler, Io, Read as StreamRead};
use shards_remote::server::{self, Host, Running, Stopper};
use shards_remote::tls;
use shards_testdir::TempDir;

/// How long anything here may take before the test fails rather than waits.
const DEADLINE: Duration = Duration::from_secs(60);

/// What the machine saw of a command, known by its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Started(String),
    /// Its stdin was let go: the daemon holds it no more.
    InputClosed(String),
    HungUp(String),
    Exited(String, i32),
    Resized(String, u16, u16),
    Signalled(String, u32),
    /// The daemon let go of its stream's state, the means to hang it up with it.
    Released(String),
    /// Its start is under way, held until the test lets it go (`stalls`).
    Stalled(String),
}

/// A machine whose clients and invitations are kept in memory, running each command as a
/// process of its own, and saying what it saw of each.
struct Machine {
    clients: Mutex<Vec<(String, Vec<u8>)>>,
    invitations: Mutex<Vec<Invitation>>,
    seen: mpsc::Sender<Seen>,
    /// What lets a `stalls` command's start go on.
    stall: Mutex<Option<mpsc::Receiver<()>>>,
    /// What lets a join go on, where one is held.
    join_stall: Mutex<Option<mpsc::Receiver<()>>>,
    /// How long it takes to say whom a key is, on the driver at each handshake's end: a host
    /// slow to answer a fresh client's first ask.
    enroll_stall: Duration,
}

/// What a [`Machine`] holds up, and how: a `stalls` command's start, every join, and each
/// handshake's end.
#[derive(Default)]
struct Stalls {
    start: Option<mpsc::Receiver<()>>,
    join: Option<mpsc::Receiver<()>>,
    enroll: Duration,
}

/// A command's stdin, which says when it is let go.
struct Told {
    stdin: std::process::ChildStdin,
    what: String,
    seen: mpsc::Sender<Seen>,
}

impl std::io::Write for Told {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stdin.flush()
    }
}

impl Drop for Told {
    fn drop(&mut self) {
        let _ = self.seen.send(Seen::InputClosed(self.what.clone()));
    }
}

/// What a command's hangup holds, which says when the daemon lets go of it.
struct Held(mpsc::Sender<Seen>, String);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.send(Seen::Released(self.1.clone()));
    }
}

impl Host for Machine {
    fn enrolled(&self, spki: &[u8]) -> Option<String> {
        std::thread::sleep(self.enroll_stall);
        let clients = self.clients.lock().unwrap();
        clients
            .iter()
            .find(|(_, key)| key == spki)
            .map(|(name, _)| name.clone())
    }

    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error> {
        if let Some(release) = self.join_stall.lock().unwrap().as_ref() {
            let _ = self.seen.send(Seen::Stalled(format!("join {name}")));
            let _ = release.recv();
        }
        let mut invitations = self.invitations.lock().unwrap();
        let at = invitations
            .iter()
            .position(|i| i.name == name && invite::verify(&i.secret, exported, proof))
            .ok_or_else(|| Error::new("no"))?;
        invitations.remove(at);
        self.clients
            .lock()
            .unwrap()
            .push((name.to_string(), spki.to_vec()));
        Ok(())
    }

    fn start(&self, _client: &str, request: Request) -> Result<Running, Error> {
        let args: Vec<String> = request
            .args
            .iter()
            .map(|a| String::from_utf8(a.clone()).unwrap())
            .collect();
        // `made-late COMMAND...`: COMMAND, which tells what it made only once it has exited.
        let late = args.first().is_some_and(|a| a == "made-late");
        let args = if late { args[1..].to_vec() } else { args };
        // `outlives-hangup COMMAND...`: COMMAND, which goes on through its hangup, as one
        // that ignores SIGHUP does.
        let outlives = args.first().is_some_and(|a| a == "outlives-hangup");
        let args = if outlives { args[1..].to_vec() } else { args };
        // `stalls COMMAND...`: COMMAND, its start held until the test lets it go, as a host's
        // start that takes long would hold it.
        let stalls = args.first().is_some_and(|a| a == "stalls");
        let args = if stalls { args[1..].to_vec() } else { args };
        if stalls {
            let _ = self.seen.send(Seen::Stalled(args.join(" ")));
            if let Some(release) = self.stall.lock().unwrap().as_ref() {
                let _ = release.recv();
            }
        }
        let (gone, gone_rx) = mpsc::channel::<()>();
        let what = args.join(" ");
        // Each command one of the tests' helper program's, the same on every host.
        let mut child = Command::new(shards_testdir::helper().map_err(Error::new)?)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(e.to_string()))?;
        let stdin = Told {
            stdin: child.stdin.take().unwrap(),
            what: what.clone(),
            seen: self.seen.clone(),
        };
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let _ = self.seen.send(Seen::Started(what.clone()));
        let (exited, hung) = (self.seen.clone(), self.seen.clone());
        let hung_what = what.clone();
        let held = Held(self.seen.clone(), what.clone());
        let (resized, signalled) = (self.seen.clone(), self.seen.clone());
        let (resized_what, signalled_what) = (what.clone(), what.clone());
        // A hangup ends the command, unless it outlives one: the wait kills it as it is
        // told, which holds the child.
        let (hang_up, hung_up) = mpsc::channel::<()>();
        Ok(Running {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            wait: Box::new(move || {
                let ended = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {}
                        Err(e) => break Err(e),
                    }
                    if hung_up.try_recv().is_ok() && !outlives {
                        let _ = child.kill();
                    }
                    std::thread::sleep(Duration::from_millis(5));
                };
                let status = ended.map(|s| s.code().unwrap_or(128)).unwrap_or(1);
                let _ = exited.send(Seen::Exited(what, status));
                let _ = gone.send(());
                status
            }),
            hangup: Box::new(move || {
                let _ = &held;
                let _ = hung.send(Seen::HungUp(hung_what.clone()));
                let _ = hang_up.send(());
            }),
            resize: Some(Box::new(move |rows, cols| {
                let _ = resized.send(Seen::Resized(resized_what.clone(), rows, cols));
            })),
            signal: Box::new(move |linux| {
                let _ = signalled.send(Seen::Signalled(signalled_what.clone(), linux));
            }),
            made: late.then(|| {
                Box::new(Late {
                    gone: gone_rx,
                    id: Some(b"made-id".to_vec()),
                }) as Box<dyn Read + Send>
            }),
        })
    }

    fn invited(&self) -> usize {
        self.invitations.lock().unwrap().len()
    }

    fn handshakes(&self) -> usize {
        usize::MAX
    }
}

/// What a machine saw, kept as it comes, so that a wait for one thing loses none of the rest.
struct Watch {
    rx: mpsc::Receiver<Seen>,
    seen: Vec<Seen>,
}

impl Watch {
    /// Waits until the machine has seen `what`.
    fn saw(&mut self, what: &Seen) {
        let until = Instant::now() + DEADLINE;
        while !self.seen.contains(what) {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(seen) => self.seen.push(seen),
                Err(_) => panic!("the machine never saw {what:?}; it saw {:?}", self.seen),
            }
        }
    }

    /// What the machine has seen by now.
    fn now(&mut self) -> &[Seen] {
        while let Ok(seen) = self.rx.try_recv() {
            self.seen.push(seen);
        }
        &self.seen
    }
}

/// A daemon serving a [`Machine`] on loopback, on a thread of its own, stopped as it drops,
/// as it does when a test fails: the test fails, rather than waits.
struct Daemon {
    remote: Remote,
    stopper: Stopper,
    seen: Watch,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.stopper.stop();
    }
}

fn daemon(
    identity: &'static Identity,
    clients: Vec<(String, Vec<u8>)>,
    invitations: Vec<Invitation>,
) -> Daemon {
    daemon_with(identity, clients, invitations, Stalls::default())
}

/// [`daemon`], held up as `stalls` says.
fn daemon_with(
    identity: &'static Identity,
    clients: Vec<(String, Vec<u8>)>,
    invitations: Vec<Invitation>,
    stalls: Stalls,
) -> Daemon {
    // The commands' program, built before the daemon serves: its first start would build it
    // (cargo, seconds) on the daemon's driver, which answers nothing meanwhile, and a client
    // whose send goes unanswered a whole window past its probe timeout is let go (D180).
    shards_testdir::helper().unwrap();
    let (seen, rx) = mpsc::channel();
    let machine = Machine {
        clients: Mutex::new(clients),
        invitations: Mutex::new(invitations),
        seen,
        stall: Mutex::new(stalls.start),
        join_stall: Mutex::new(stalls.join),
        enroll_stall: stalls.enroll,
    };
    let (tx, bound) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = server::serve(
            identity,
            "127.0.0.1:0".parse().unwrap(),
            machine,
            |at, stopper| {
                let _ = tx.send((at, stopper));
            },
        );
    });
    let (address, stopper) = bound.recv_timeout(DEADLINE).unwrap();
    Daemon {
        remote: Remote {
            address,
            daemon: identity.fingerprint(),
            known: tls::Known::Never,
        },
        stopper,
        seen: Watch { rx, seen: Vec::new() },
    }
}

fn leak(identity: Identity) -> &'static Identity {
    Box::leak(Box::new(identity))
}

/// What a writer the command's output goes to holds, once the command has ended.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

/// What a command tells of what it made, only once it has exited, and later than its exit's
/// own frame takes: the order a busy host may run them in.
struct Late {
    gone: mpsc::Receiver<()>,
    id: Option<Vec<u8>>,
}

impl Read for Late {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(id) = self.id.take() else {
            return Ok(0);
        };
        let _ = self.gone.recv();
        std::thread::sleep(Duration::from_millis(50));
        let n = id.len().min(buf.len());
        buf[..n].copy_from_slice(&id[..n]);
        Ok(n)
    }
}

type Ran = (Result<i32, String>, Vec<u8>, Vec<u8>);

/// Runs `args` on `remote` as `identity`, `stdin` sent where given, on a thread of its own:
/// what [`Pending::get`] gives once it has ended.
fn run(
    identity: &'static Identity,
    remote: Remote,
    args: &[&str],
    stdin: Option<Box<dyn Read + Send>>,
) -> Pending<Ran> {
    run_steered(identity, remote, args, stdin, None)
}

/// [`run`], what steers the command given to `steered`.
fn run_steered(
    identity: &'static Identity,
    remote: Remote,
    args: &[&str],
    stdin: Option<Box<dyn Read + Send>>,
    steered: Option<mpsc::Sender<Controls>>,
) -> Pending<Ran> {
    let args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (out, err) = (Captured::default(), Captured::default());
        let request = Request {
            args,
            env: Vec::new(),
            terminal: None,
        };
        let steer = |controls| {
            if let Some(steered) = steered {
                let _ = steered.send(controls);
            }
        };
        let status = client::exec(identity, remote, request, stdin, out.clone(), err.clone(), steer).unwrap();
        let _ = tx.send((status, out.bytes(), err.bytes()));
    });
    Pending(rx)
}

/// What a thread of a test will give.
struct Pending<T>(mpsc::Receiver<T>);

impl<T> Pending<T> {
    fn get(self) -> T {
        self.0
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("not ended within {DEADLINE:?}"))
    }
}

fn input(bytes: &[u8]) -> Option<Box<dyn Read + Send>> {
    Some(Box::new(std::io::Cursor::new(bytes.to_vec())))
}

#[test]
fn a_client_joins_then_runs_commands_whose_output_crosses_whole() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let stranger = leak(Identity::generate().unwrap());
    // The invitation is made before the daemon serves: the address is filled in after.
    let pending = Invitation::new("laptop", "127.0.0.1:1".parse().unwrap(), daemon_id.fingerprint()).unwrap();
    let d = daemon(daemon_id, Vec::new(), vec![pending.clone()]);
    let remote = d.remote;
    let invitation = Invitation {
        address: remote.address,
        ..pending
    };

    // Not enrolled: refused.
    let (status, _, _) = run(laptop, remote, &["true"], None).get();
    assert_eq!(status, Err(server::NOT_ENROLLED.to_string()));
    // Another secret: refused, in the same words as any join that fails.
    let forged = Invitation {
        secret: [9; 32],
        ..invitation.clone()
    };
    assert_eq!(
        client::join(stranger, &forged).0.unwrap(),
        Err(server::NOT_INVITED.to_string())
    );
    // The invitation: enrolled, once.
    assert_eq!(client::join(laptop, &invitation).0.unwrap(), Ok(()));
    assert_eq!(
        client::join(stranger, &invitation).0.unwrap(),
        Err(server::NOT_INVITED.to_string())
    );

    let (status, out, err) = run(laptop, remote, &["say", "out", "err", "3"], None).get();
    assert_eq!(status, Ok(3));
    assert_eq!(out, b"out\n");
    assert_eq!(err, b"err\n");
    let (status, out, _) = run(laptop, remote, &["cat"], input(b"hello over quic")).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out, b"hello over quic");
    // Eight MiB of stdin, more than its pipe and the daemon's room for it: it crosses whole.
    let (status, out, _) = run(laptop, remote, &["cat"], input(&vec![7; 8 << 20])).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out.len(), 8 << 20);
    assert!(out.iter().all(|b| *b == 7));
    // Eight MiB, more than a stream's window: it crosses whole.
    let (status, out, _) = run(laptop, remote, &["zeros", "8388608"], None).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out.len(), 8 << 20);
    assert!(out.iter().all(|b| *b == 0));
    // Still not enrolled.
    let (status, _, _) = run(stranger, remote, &["true"], None).get();
    assert_eq!(status, Err(server::NOT_ENROLLED.to_string()));
}

/// A daemon that answers each request with `answer`, written at once with its stream's end.
struct Answers {
    answer: Vec<u8>,
    /// What each stream has sent, by its connection too: every connection's first stream
    /// has the same number, and a frame one left must never be read as the next one's.
    inbound: HashMap<(ConnKey, StreamId), Vec<u8>>,
}

impl Handler for Answers {
    type Command = ();

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.readable(io, conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let buffer = self.inbound.entry((conn, stream)).or_default();
        loop {
            match io.read(conn, stream) {
                StreamRead::Bytes(bytes) => buffer.extend_from_slice(&bytes),
                StreamRead::End(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    break;
                }
                StreamRead::Blocked | StreamRead::Reset => break,
            }
        }
        // Every whole frame, the request's stdin's end among them, which comes with it.
        while let Ok(Some((tag, _))) = frame::take(buffer) {
            if tag == Tag::Exec {
                assert_eq!(io.write(conn, stream, &self.answer).unwrap(), self.answer.len());
                io.finish(conn, stream).unwrap();
            }
        }
    }

    fn writable(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        self.inbound.retain(|(c, _), _| *c != conn);
    }
    fn command(&mut self, _io: &mut Io<'_>, _command: ()) {}

    fn done(&self) -> bool {
        false
    }
}

/// A driver's sender that stops it as it drops.
struct Stops(net::Sender<()>);

impl Drop for Stops {
    fn drop(&mut self) {
        let _ = self.0.send(net::Command::Stop);
    }
}

/// A command's every frame and its stream's end, come at once, are all taken: its output
/// written whole and its status given, as the end is judged after all the stream carried.
#[test]
fn a_streams_last_frames_and_its_end_come_at_once_and_are_all_taken() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut answer = Vec::new();
    frame::put(&mut answer, Tag::Stdout, b"out\n").unwrap();
    frame::put(&mut answer, Tag::Stderr, b"err\n").unwrap();
    frame::put(&mut answer, Tag::Exit, &3i32.to_be_bytes()).unwrap();
    let (tx, bound) = mpsc::channel();
    std::thread::spawn(move || {
        let handler = Answers {
            answer,
            inbound: HashMap::new(),
        };
        let config = tls::server(daemon_id).unwrap();
        let (mut driver, sender) = Driver::bind(
            "127.0.0.1:0".parse().unwrap(),
            Some(config),
            handler,
            tls::Known::Never,
        )
        .unwrap();
        let _ = tx.send((driver.local().unwrap(), sender));
        let _ = driver.run();
    });
    let (address, sender) = bound.recv_timeout(DEADLINE).unwrap();
    let _stops = Stops(sender);
    let remote = Remote {
        address,
        daemon: daemon_id.fingerprint(),
        known: tls::Known::Never,
    };
    for _ in 0..20 {
        let (status, out, err) = run(laptop, remote, &["anything"], None).get();
        assert_eq!(status, Ok(3));
        assert_eq!(out, b"out\n");
        assert_eq!(err, b"err\n");
    }
}

/// A daemon that reads none of a request's stdin (STOP_SENDING at once), and answers each
/// request when the test says.
struct Unread {
    asked: mpsc::Sender<(ConnKey, StreamId)>,
    /// What each stream has sent, by its connection too (`Answers`).
    inbound: HashMap<(ConnKey, StreamId), Vec<u8>>,
}

impl Handler for Unread {
    type Command = (ConnKey, StreamId);

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.readable(io, conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let buffer = self.inbound.entry((conn, stream)).or_default();
        while let StreamRead::Bytes(bytes) = io.read(conn, stream) {
            buffer.extend_from_slice(&bytes);
        }
        while let Ok(Some((tag, _))) = frame::take(buffer) {
            if tag == Tag::Exec {
                io.stop(conn, stream);
                let _ = self.asked.send((conn, stream));
            }
        }
    }

    fn writable(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        self.inbound.retain(|(c, _), _| *c != conn);
    }

    fn command(&mut self, io: &mut Io<'_>, (conn, stream): (ConnKey, StreamId)) {
        let mut answer = Vec::new();
        frame::put(&mut answer, Tag::Exit, &5i32.to_be_bytes()).unwrap();
        assert_eq!(io.write(conn, stream, &answer).unwrap(), answer.len());
        io.finish(conn, stream).unwrap();
    }

    fn done(&self) -> bool {
        false
    }
}

/// A daemon that reads no more of a client's stdin ends none of its command: the client
/// sends the rest nowhere, and waits for the answer, which comes after.
#[test]
fn a_client_asked_to_send_no_more_stdin_waits_for_its_answer() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let (tx, bound) = mpsc::channel();
    let (asked_tx, asked) = mpsc::channel();
    std::thread::spawn(move || {
        let handler = Unread {
            asked: asked_tx,
            inbound: HashMap::new(),
        };
        let config = tls::server(daemon_id).unwrap();
        let (mut driver, sender) = Driver::bind(
            "127.0.0.1:0".parse().unwrap(),
            Some(config),
            handler,
            tls::Known::Never,
        )
        .unwrap();
        let _ = tx.send((driver.local().unwrap(), sender));
        let _ = driver.run();
    });
    let (address, sender) = bound.recv_timeout(DEADLINE).unwrap();
    let remote = Remote {
        address,
        daemon: daemon_id.fingerprint(),
        known: tls::Known::Never,
    };
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let later = run(laptop, remote, &["anything"], endless);
    let (conn, stream) = asked.recv_timeout(DEADLINE).unwrap();
    // Its client sends stdin on meanwhile, to a stream stopped.
    std::thread::sleep(Duration::from_millis(200));
    sender.send(net::Command::Handler((conn, stream))).unwrap();
    let (status, _, _) = later.get();
    assert_eq!(status, Ok(5));
    let _ = sender.send(net::Command::Stop);
}

/// What steers a command reaches it on a stream of its own, though its stdin waits for room
/// it does not get: its terminal's size, and the signals its client receives.
#[test]
fn what_steers_a_command_passes_its_stdin_waiting_for_room() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let dir = TempDir::new("remote-steer").unwrap();
    let go = dir.path().join("go");
    // Each command that waits on the test waits for it a while alone (the helper's
    // `await`): one whose test failed, or was killed, before it wrote what it waits for is
    // no command left behind for long.
    let go = go.to_str().unwrap();
    let what = format!("await {go}");
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let (steered, controls) = mpsc::channel();
    let later = run_steered(laptop, d.remote, &["await", go], endless, Some(steered));
    let controls = controls.recv_timeout(DEADLINE).unwrap();
    d.seen.saw(&Seen::Started(what.clone()));
    // Long enough for its pipe, and the daemon's room for its stdin, to fill.
    std::thread::sleep(Duration::from_millis(300));
    controls.resize(40, 120).unwrap();
    controls.signal(10).unwrap();
    d.seen.saw(&Seen::Resized(what.clone(), 40, 120));
    d.seen.saw(&Seen::Signalled(what, 10));
    std::fs::write(go, b"").unwrap();
    let (status, _, _) = later.get();
    assert_eq!(status, Ok(0));
}

/// A command's stdin ends as its client says: at once, for a client that sends none; and a
/// command that ends without reading it all ends its client all the same, the rest unsent.
#[test]
fn a_commands_stdin_ends_as_its_client_says_or_with_the_command() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let (status, out, _) = run(laptop, d.remote, &["cat"], None).get();
    assert_eq!(status, Ok(0));
    assert!(out.is_empty());
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let (status, _, _) = run(laptop, d.remote, &["true"], endless).get();
    assert_eq!(status, Ok(0));
}

/// A command that reads none of the stdin its client sends holds up no other command: the
/// daemon reads its stream no further, as its stdin has no room, and serves the rest.
#[test]
fn a_command_that_reads_no_stdin_holds_up_no_other() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let dir = TempDir::new("remote-held").unwrap();
    let go = dir.path().join("go");
    let go = go.to_str().unwrap();
    let endless: Option<Box<dyn Read + Send>> = Some(Box::new(std::io::repeat(b'y')));
    let held = run(laptop, d.remote, &["await", go], endless);
    d.seen.saw(&Seen::Started(format!("await {go}")));
    // Long enough for its pipe, and the daemon's room for its stdin, to fill.
    std::thread::sleep(Duration::from_millis(300));
    let (status, out, _) = run(laptop, d.remote, &["echo", "hi"], None).get();
    assert_eq!(status, Ok(0));
    assert_eq!(out, b"hi\n");
    std::fs::write(go, b"").unwrap();
    let (status, _, _) = held.get();
    assert_eq!(status, Ok(0));
}

/// What a raw client hears.
#[derive(Debug, PartialEq, Eq)]
enum Heard {
    Connected,
    Frame(StreamId, Tag, Vec<u8>),
    /// Its connection ended, its peer's error code with it where its peer closed it.
    Lost(Option<u64>),
}

/// What a test asks of a raw client.
enum Ask {
    Connect(SocketAddr),
    /// Opens a stream, writes these bytes to it, finishing it if asked, and tells its id.
    Open(Vec<u8>, bool, mpsc::Sender<StreamId>),
    Reset(StreamId),
    Stop(StreamId),
    Close,
}

/// A client that sends the bytes a test gives it, keeps its connection as long as the test
/// does, and tells the test each frame it is sent.
struct Raw {
    config: Option<hyper_quic::ClientConfig>,
    heard: mpsc::Sender<Heard>,
    /// The streams the daemon reads no more of (STOP_SENDING).
    stops: mpsc::Sender<StreamId>,
    conn: Option<ConnKey>,
    inbound: HashMap<StreamId, Vec<u8>>,
    done: bool,
}

impl Handler for Raw {
    type Command = Ask;

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        let _ = self.heard.send(Heard::Connected);
        Ok(())
    }

    fn opened(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let buffer = self.inbound.entry(stream).or_default();
        loop {
            match io.read(conn, stream) {
                StreamRead::Bytes(bytes) => buffer.extend_from_slice(&bytes),
                StreamRead::End(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    break;
                }
                StreamRead::Blocked | StreamRead::Reset => break,
            }
        }
        while let Ok(Some((tag, payload))) = frame::take(buffer) {
            let _ = self.heard.send(Heard::Frame(stream, tag, payload));
        }
    }

    fn writable(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}

    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, stream: StreamId) {
        let _ = self.stops.send(stream);
    }

    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _why: &str, code: Option<u64>) {
        let _ = self.heard.send(Heard::Lost(code));
        self.done = true;
    }

    fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
        match ask {
            Ask::Connect(to) => {
                let config = self.config.take().unwrap();
                self.conn = Some(io.connect(to, config).unwrap());
            }
            Ask::Open(bytes, finish, told) => {
                let conn = self.conn.unwrap();
                let stream = io.open(conn).unwrap();
                assert_eq!(io.write(conn, stream, &bytes).unwrap(), bytes.len());
                if finish {
                    io.finish(conn, stream).unwrap();
                }
                let _ = told.send(stream);
            }
            Ask::Reset(stream) => io.reset(self.conn.unwrap(), stream),
            Ask::Stop(stream) => io.stop(self.conn.unwrap(), stream),
            Ask::Close => {
                io.close(self.conn.unwrap(), 0, "");
                self.done = true;
            }
        }
    }

    fn done(&self) -> bool {
        self.done
    }
}

/// A raw client of `remote`, as `identity`, connected, on a thread of its own.
struct RawClient {
    ask: net::Sender<Ask>,
    heard: mpsc::Receiver<Heard>,
    stops: mpsc::Receiver<StreamId>,
}

impl RawClient {
    fn new(identity: &'static Identity, remote: Remote) -> RawClient {
        let (heard_tx, heard) = mpsc::channel();
        let (stops_tx, stops) = mpsc::channel();
        let (tx, made) = mpsc::channel();
        std::thread::spawn(move || {
            let raw = Raw {
                config: Some(tls::client(identity, remote.daemon, remote.known).unwrap()),
                heard: heard_tx,
                stops: stops_tx,
                conn: None,
                inbound: HashMap::new(),
                done: false,
            };
            let (mut driver, sender) =
                Driver::bind("127.0.0.1:0".parse().unwrap(), None, raw, remote.known).unwrap();
            let _ = tx.send(sender);
            let _ = driver.run();
        });
        let ask = made.recv_timeout(DEADLINE).unwrap();
        ask.send(net::Command::Handler(Ask::Connect(remote.address)))
            .unwrap();
        let client = RawClient { ask, heard, stops };
        assert_eq!(client.next(), Heard::Connected);
        client
    }

    fn next(&self) -> Heard {
        self.heard
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("heard nothing within {DEADLINE:?}"))
    }

    /// Opens a stream with `bytes` on it; its id.
    fn open(&self, bytes: Vec<u8>) -> StreamId {
        self.opens(bytes, false)
    }

    /// Opens a stream with `bytes` on it, finished if `finish`; its id.
    fn opens(&self, bytes: Vec<u8>, finish: bool) -> StreamId {
        let (told, id) = mpsc::channel();
        self.ask
            .send(net::Command::Handler(Ask::Open(bytes, finish, told)))
            .unwrap();
        id.recv_timeout(DEADLINE).unwrap()
    }

    fn ask(&self, ask: Ask) {
        self.ask.send(net::Command::Handler(ask)).unwrap();
    }

    /// Waits until the daemon has asked it to send no more of `stream`.
    fn stopped(&self, stream: StreamId) {
        let until = Instant::now() + DEADLINE;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.stops.recv_timeout(left) {
                Ok(stopped) if stopped == stream => return,
                Ok(_) => {}
                Err(_) => panic!("{stream} not stopped within {DEADLINE:?}"),
            }
        }
    }
}

impl Drop for RawClient {
    fn drop(&mut self) {
        let _ = self.ask.send(net::Command::Stop);
    }
}

/// Waits until `path` is there.
fn until_there(path: &std::path::Path) {
    let until = Instant::now() + DEADLINE;
    while !path.exists() {
        assert!(
            Instant::now() < until,
            "{} not there within {DEADLINE:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A command refused after it started is told apart from one refused before it, which says
/// nothing of it ran: a frame out of place once its command runs ends the stream with its
/// cut, which its client takes for "it may have run" (D176 part 6).
#[test]
fn a_command_refused_after_it_started_is_cut() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let mut bytes = exec(&["cat"]);
    // A daemon's frame, which no client sends: out of place once its command runs.
    frame::put(&mut bytes, Tag::Joined, &[]).unwrap();
    let stream = raw.opens(bytes, false);
    assert_eq!(
        raw.next(),
        Heard::Frame(stream, Tag::Cut, b"a frame out of place (Joined)".to_vec())
    );
    // Refused before any command of it started, a stream still hears a refusal.
    let mut early = Vec::new();
    frame::put(&mut early, Tag::Joined, &[]).unwrap();
    let stream = raw.opens(early, false);
    assert_eq!(
        raw.next(),
        Heard::Frame(stream, Tag::Refused, b"a frame out of place (Joined)".to_vec())
    );
}

/// `args`' request frame, and none of its stdin's end.
fn exec(args: &[&str]) -> Vec<u8> {
    let request = Request {
        args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        env: Vec::new(),
        terminal: None,
    };
    let mut out = Vec::new();
    frame::put(&mut out, Tag::Exec, &request.encode().unwrap()).unwrap();
    out
}

/// A command's state goes with it: once its exit is sent, the daemon lets go of its stdin,
/// though its client neither ended that nor its connection, which runs another command.
#[test]
fn a_commands_state_goes_with_it_while_its_connection_stays() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let first = raw.open(exec(&["true"]));
    assert_eq!(
        raw.next(),
        Heard::Frame(first, Tag::Exit, 0i32.to_be_bytes().to_vec())
    );
    // What it still sends goes unread: it is asked to send no more.
    raw.stopped(first);
    d.seen.saw(&Seen::InputClosed("true".into()));
    d.seen.saw(&Seen::Released("true".into()));
    let second = raw.open(exec(&["echo", "again"]));
    assert_eq!(raw.next(), Heard::Frame(second, Tag::Stdout, b"again\n".to_vec()));
    assert_eq!(
        raw.next(),
        Heard::Frame(second, Tag::Exit, 0i32.to_be_bytes().to_vec())
    );
    d.seen.saw(&Seen::InputClosed("echo again".into()));
    d.seen.saw(&Seen::Released("echo again".into()));
    raw.ask(Ask::Close);
}

/// A client that finishes its stream with the last of its stdin ends the command's input
/// there: the command reads all of it, then its end, and its exit crosses back.
#[test]
fn a_client_that_finishes_its_stream_ends_its_commands_input() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let mut bytes = exec(&["cat"]);
    frame::put(&mut bytes, Tag::Stdin, b"the last of it").unwrap();
    let stream = raw.opens(bytes, true);
    assert_eq!(
        raw.next(),
        Heard::Frame(stream, Tag::Stdout, b"the last of it".to_vec())
    );
    assert_eq!(
        raw.next(),
        Heard::Frame(stream, Tag::Exit, 0i32.to_be_bytes().to_vec())
    );
}

/// A client the machine has not enrolled is refused by its frame's header: neither a
/// command's bytes nor more than a join's are waited for.
#[test]
fn a_client_not_enrolled_is_refused_by_its_frames_header() {
    let daemon_id = leak(Identity::generate().unwrap());
    let stranger = leak(Identity::generate().unwrap());
    let d = daemon(daemon_id, Vec::new(), vec![invitation(daemon_id)]);
    let raw = RawClient::new(stranger, d.remote);
    let header = |tag: Tag, len: usize| {
        let mut out = vec![tag as u8];
        out.extend_from_slice(&u32::try_from(len).unwrap().to_be_bytes());
        out
    };
    let command = raw.open(header(Tag::Exec, frame::MAX_FRAME));
    assert_eq!(
        raw.next(),
        Heard::Frame(command, Tag::Refused, server::NOT_ENROLLED.as_bytes().to_vec())
    );
    let join = raw.open(header(Tag::Join, invite::JOIN_MAX + 1));
    assert_eq!(
        raw.next(),
        Heard::Frame(join, Tag::Refused, server::NOT_INVITED.as_bytes().to_vec())
    );
    let controls = raw.open(header(Tag::Controls, 8));
    assert_eq!(
        raw.next(),
        Heard::Frame(
            controls,
            Tag::Refused,
            b"a frame out of place (Controls)".to_vec()
        )
    );
}

/// A control stream steers a command of its own connection's, or none: one that names none
/// is refused.
#[test]
fn a_control_stream_for_no_command_is_refused() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let mut named = Vec::new();
    frame::put(&mut named, Tag::Controls, &96u64.to_be_bytes()).unwrap();
    let stream = raw.open(named);
    assert_eq!(
        raw.next(),
        Heard::Frame(
            stream,
            Tag::Refused,
            b"no command of this connection's to steer".to_vec()
        )
    );
}

/// An invitation of the daemon `of`, for no one in particular.
fn invitation(of: &Identity) -> Invitation {
    Invitation::new("someone", "127.0.0.1:1".parse().unwrap(), of.fingerprint()).unwrap()
}

/// A client's refusal at its handshake's end, the daemon's code.
fn unenrolled() -> Heard {
    Heard::Lost(Some(u64::from(server::UNENROLLED)))
}

/// The machine holds as many clients it has not enrolled at once as it holds invitations, each
/// of which enrolls one, with no deadline of its own: a newer stranger takes the place of the
/// oldest, which is let go; a machine that holds none takes none, refused as its handshake ends.
#[test]
fn a_machine_holds_as_many_strangers_as_invitations_the_oldest_giving_way() {
    let daemon_id = leak(Identity::generate().unwrap());
    let strangers: Vec<&'static Identity> = (0..3).map(|_| leak(Identity::generate().unwrap())).collect();
    let d = daemon(daemon_id, Vec::new(), vec![invitation(daemon_id)]);
    let first = RawClient::new(strangers[0], d.remote);
    // The first is served: a join too long, refused by its header.
    let mut join = vec![Tag::Join as u8];
    join.extend_from_slice(&u32::try_from(invite::JOIN_MAX + 1).unwrap().to_be_bytes());
    let stream = first.open(join.clone());
    assert_eq!(
        first.next(),
        Heard::Frame(stream, Tag::Refused, server::NOT_INVITED.as_bytes().to_vec())
    );
    // A newer stranger takes its place: the first let go, the newer served.
    let second = RawClient::new(strangers[1], d.remote);
    assert_eq!(first.next(), unenrolled());
    let stream = second.open(join);
    assert_eq!(
        second.next(),
        Heard::Frame(stream, Tag::Refused, server::NOT_INVITED.as_bytes().to_vec())
    );
    // A machine that holds no invitation takes none.
    let none = daemon(daemon_id, Vec::new(), Vec::new());
    let third = RawClient::new(strangers[2], none.remote);
    assert_eq!(third.next(), unenrolled());
}

/// A host's start runs apart from the daemon's driver, which acknowledges every client
/// meanwhile: while one start is held (a stand-in for one that takes long), another client's
/// command is answered whole, and the held one, let go, runs as any other.
#[test]
fn a_slow_start_holds_up_no_other_client() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let (release, held) = mpsc::channel();
    let mut d = daemon_with(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
        Stalls {
            start: Some(held),
            ..Stalls::default()
        },
    );
    let slow = run(laptop, d.remote, &["stalls", "say", "slow", "x", "0"], None);
    d.seen.saw(&Seen::Stalled("say slow x 0".into()));
    let (status, out, _) = run(laptop, d.remote, &["say", "quick", "x", "0"], None).get();
    assert_eq!((status, out), (Ok(0), b"quick\n".to_vec()));
    release.send(()).unwrap();
    let (status, out, _) = slow.get();
    assert_eq!((status, out), (Ok(0), b"slow\n".to_vec()));
}

/// A host's join runs apart from the daemon's driver too, which acknowledges every client
/// meanwhile: while a stranger's join is held (a stand-in for one whose record takes long to
/// sync), an enrolled client's command is answered whole, and the join, let go, enrolls its
/// stranger. Red with the join on the driver: tests/context.rs's joins on HVF (ROOT's run at
/// dc7f45bc6) died at 123-152 ms, their client's send unanswered while the daemon synced.
#[test]
fn a_slow_join_holds_up_no_other_client() {
    let daemon_id = leak(Identity::generate().unwrap());
    let (laptop, stranger) = (
        leak(Identity::generate().unwrap()),
        leak(Identity::generate().unwrap()),
    );
    let (release, held) = mpsc::channel();
    let invited = invitation(daemon_id);
    let mut d = daemon_with(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        vec![invited.clone()],
        Stalls {
            join: Some(held),
            ..Stalls::default()
        },
    );
    let invitation = Invitation {
        address: d.remote.address,
        ..invited
    };
    // A thread of its own, which a failing test does not wait for (the join it holds would
    // never be let go).
    let (joined_tx, joined) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = joined_tx.send(client::join(stranger, &invitation).0);
    });
    d.seen.saw(&Seen::Stalled("join someone".into()));
    let (status, out, _) = run(laptop, d.remote, &["say", "quick", "x", "0"], None).get();
    assert_eq!((status, out), (Ok(0), b"quick\n".to_vec()));
    release.send(()).unwrap();
    assert_eq!(joined.recv_timeout(DEADLINE).unwrap().unwrap(), Ok(()));
}

/// A daemon slow to answer a fresh client's first ask, its driver held a while as it says whom
/// the key is (a stand-in for a host busy, or scheduled off, as the client asks), is waited for:
/// cold, before any answer of a send's has been seen on its path, the client's wait has no
/// window, a handshake's own being no measure of a request's (D180). Red with the handshake
/// seeding the window: the client's send unanswered past it, its connection taken for dead.
#[test]
fn a_daemon_slow_to_answer_a_fresh_client_is_waited_for() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon_with(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
        Stalls {
            enroll: Duration::from_millis(300),
            ..Stalls::default()
        },
    );
    let (status, out, _) = run(laptop, d.remote, &["say", "late", "x", "0"], None).get();
    assert_eq!((status, out), (Ok(0), b"late\n".to_vec()));
}

/// A client the machine enrolled keeps its connection through its command's silence: neither
/// side keeps it alive on a path where none was found dead, and neither has an idle timer.
#[test]
fn an_enrolled_clients_silent_command_lives_with_no_keep_alive() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let (status, _, _) = run(laptop, d.remote, &["sleep", "2"], None).get();
    assert_eq!(status, Ok(0));
}

/// Asks `true` of `remote` as `client::exec_with` does, giving up the handshake at the probe
/// `give_up` says to: its answer, every probe told, and what its connection saw.
fn asked(
    remote: Remote,
    mut give_up: impl FnMut(client::Unanswered) -> bool,
) -> (
    Result<Result<i32, String>, Error>,
    Vec<client::Unanswered>,
    net::Seen,
) {
    let laptop = leak(Identity::generate().unwrap());
    let request = Request {
        args: vec![b"true".to_vec()],
        env: Vec::new(),
        terminal: None,
    };
    let mut told = Vec::new();
    let (ran, seen) = client::exec_with(
        laptop,
        remote,
        request,
        None,
        std::io::sink(),
        std::io::sink(),
        |_| {},
        |_| {},
        |unanswered| {
            told.push(unanswered);
            !give_up(unanswered)
        },
        |_| {},
    );
    (ran, told, seen)
}

/// A daemon never reached that answers nothing is probed as QUIC's loss recovery probes
/// (RFC 9002 §6.2), each probe told its caller, typed, for as long as its caller waits: no
/// deadline of shards' own ends it, and the caller's giving up does.
#[test]
fn no_daemon_answering_is_probed_until_its_caller_gives_up() {
    // A socket that takes datagrams and answers none.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let remote = Remote {
        address: silent.local_addr().unwrap(),
        daemon: Identity::generate().unwrap().fingerprint(),
        known: tls::Known::Never,
    };
    let (ran, told, seen) = asked(remote, |u| u.probes >= 3);
    let said = ran.unwrap_err().to_string();
    assert!(said.contains("no shards daemon answered"), "{said}");
    assert!(told.len() >= 2, "{told:?}");
    assert!(
        told.windows(2)
            .all(|w| w[0].probes < w[1].probes && w[0].waited <= w[1].waited),
        "{told:?}"
    );
    assert_eq!((seen.handshake, seen.ran_out, seen.refused), (None, false, false));
    assert_eq!(remote.known.after(&seen), tls::Known::Never);
    drop(silent);
}

/// What ends a command from another thread ends it while its handshake still waits on a daemon
/// never reached, which has no deadline of its own: its holder's end (a daemon's stop) is what
/// ends that wait, at once, not at a probe its backoff (RFC 9002 §6.2.1) puts ever further off.
#[test]
fn a_hang_up_ends_a_command_whose_daemon_never_answered() {
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let remote = Remote {
        address: silent.local_addr().unwrap(),
        daemon: Identity::generate().unwrap().fingerprint(),
        known: tls::Known::Never,
    };
    let laptop = leak(Identity::generate().unwrap());
    let request = Request {
        args: vec![b"true".to_vec()],
        env: Vec::new(),
        terminal: None,
    };
    let (held_tx, held_rx) = mpsc::channel();
    std::thread::scope(|s| {
        let asking = s.spawn(move || {
            client::exec_with(
                laptop,
                remote,
                request,
                None,
                std::io::sink(),
                std::io::sink(),
                |_| {},
                |_| {},
                |_| true,
                move |hang_up| {
                    let _ = held_tx.send(hang_up);
                },
            )
        });
        let hang_up = held_rx.recv().unwrap();
        // Its first Initial has come: the handshake waits on.
        let mut first = [0u8; 2048];
        silent.recv_from(&mut first).unwrap();
        let start = Instant::now();
        hang_up.now().unwrap();
        let (ran, seen) = asking.join().unwrap();
        let said = ran.unwrap_err().to_string();
        assert!(said.contains("hung up"), "{said}");
        assert!(start.elapsed() < DEADLINE, "{:?}", start.elapsed());
        assert_eq!(seen.handshake, None);
        assert_eq!(remote.known.after(&seen), tls::Known::Never);
    });
}

/// An address where nothing listens is refused at once: its host's ICMP port-unreachable reads
/// on the client's socket, connected to its one peer, as a definite refusal.
#[test]
fn a_closed_port_refuses_its_client_at_once() {
    let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let remote = Remote {
        address,
        daemon: Identity::generate().unwrap().fingerprint(),
        known: tls::Known::Never,
    };
    let start = Instant::now();
    let (ran, _, seen) = asked(remote, |_| false);
    let said = ran.unwrap_err().to_string();
    assert!(said.contains("refused"), "{said}");
    assert!(seen.refused, "{seen:?}");
    assert!(start.elapsed() < DEADLINE, "{:?}", start.elapsed());
}

/// A daemon reached before is given up past the slowest handshake seen from it, its run-out
/// marked, so that the next attempt waits as for a daemon never reached.
#[test]
fn a_daemon_reached_before_is_given_up_past_its_slowest_handshake() {
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let path = tls::Path {
        rtt: Duration::from_millis(1),
        handshake: Duration::from_millis(300),
        answer: Duration::from_millis(1),
        lost: None,
    };
    let remote = Remote {
        address: silent.local_addr().unwrap(),
        daemon: Identity::generate().unwrap().fingerprint(),
        known: tls::Known::Reached(path),
    };
    let start = Instant::now();
    let (ran, _, seen) = asked(remote, |_| false);
    let took = start.elapsed();
    let said = ran.unwrap_err().to_string();
    assert!(said.contains("slowest handshake seen from it"), "{said}");
    assert!(took >= path.handshake, "{took:?}");
    assert!(seen.ran_out, "{seen:?}");
    assert_eq!(remote.known.after(&seen), tls::Known::RanOut(path));
    drop(silent);
}

/// A command whose client leaves its stream, resetting it or reading no more of it, is hung
/// up, and its state let go.
#[test]
fn a_command_whose_client_leaves_its_stream_is_hung_up() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let raw = RawClient::new(laptop, d.remote);
    let reset = raw.open(exec(&["sleep", "31"]));
    d.seen.saw(&Seen::Started("sleep 31".into()));
    raw.ask(Ask::Reset(reset));
    d.seen.saw(&Seen::HungUp("sleep 31".into()));
    d.seen.saw(&Seen::InputClosed("sleep 31".into()));
    d.seen.saw(&Seen::Released("sleep 31".into()));
    let stopped = raw.open(exec(&["sleep", "32"]));
    d.seen.saw(&Seen::Started("sleep 32".into()));
    raw.ask(Ask::Stop(stopped));
    d.seen.saw(&Seen::HungUp("sleep 32".into()));
    d.seen.saw(&Seen::InputClosed("sleep 32".into()));
    d.seen.saw(&Seen::Released("sleep 32".into()));
}

/// What a command whose client has gone still writes, and its exit, reach no other client:
/// not the one whose connection came after, whatever QUIC streams the two had.
#[test]
fn what_a_command_of_a_connection_gone_writes_reaches_no_other() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let mut d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let dir = TempDir::new("remote-gone").unwrap();
    let (go, done, ready) = (
        dir.path().join("go"),
        dir.path().join("done"),
        dir.path().join("ready"),
    );
    // It goes on through the hangup its client's going brings, once it has said it will.
    let readied = format!("ready={}", ready.to_str().unwrap());
    let stale = ["await", go.to_str().unwrap(), readied.as_str(), "say=stale"];
    {
        let raw = RawClient::new(laptop, d.remote);
        let outlives: Vec<&str> = ["outlives-hangup"].into_iter().chain(stale).collect();
        raw.open(exec(&outlives));
        until_there(&ready);
        raw.ask(Ask::Close);
    }
    d.seen.saw(&Seen::HungUp(stale.join(" ")));
    // The connection gone, and drained: three probe timeouts on loopback.
    std::thread::sleep(Duration::from_millis(500));
    let fresh = ["await", done.to_str().unwrap(), "say=fresh", "exit=7"];
    let later = run(laptop, d.remote, &fresh, None);
    d.seen.saw(&Seen::Started(fresh.join(" ")));
    std::fs::write(&go, b"").unwrap();
    d.seen.saw(&Seen::Exited(stale.join(" "), 0));
    // What it wrote, and its exit, reach the daemon's driver meanwhile.
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&done, b"").unwrap();
    let (status, out, _) = later.get();
    assert_eq!(status, Ok(7), "{:?}", d.seen.now());
    assert_eq!(String::from_utf8_lossy(&out), "fresh\n");
}

/// What a command tells of the microVM it made reaches its client before the command's
/// exit status, however late it is told: here only once the command has exited.
#[test]
fn what_a_command_made_comes_before_its_exit() {
    let daemon_id = leak(Identity::generate().unwrap());
    let laptop = leak(Identity::generate().unwrap());
    let d = daemon(
        daemon_id,
        vec![("laptop".into(), laptop.spki().to_vec())],
        Vec::new(),
    );
    let request = Request {
        args: vec![b"made-late".to_vec(), b"true".to_vec()],
        env: Vec::new(),
        terminal: None,
    };
    let mut told = Vec::new();
    let status = client::exec_made(
        laptop,
        d.remote,
        request,
        None,
        std::io::sink(),
        std::io::sink(),
        |_| {},
        |id| told.extend_from_slice(id),
    )
    .unwrap();
    assert_eq!(status, Ok(0));
    assert_eq!(told, b"made-id");
}
