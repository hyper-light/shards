//! The client's half (D180): a command sent whole to another machine's daemon, its stdin sent
//! as it comes, and what the command prints written here as it comes, by threads of their
//! own, so that a reader of its output that stops a while holds up no packet the connection's
//! liveness needs. What steers the command, its terminal's size and the signals its client
//! receives, goes on a control stream of its own (`frame`), once there is any.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::mpsc;

use hyper_quic::StreamId;

use crate::frame::{self, Request, Tag};
use crate::identity::{Fingerprint, Identity};
use crate::invite::{self, Invitation};
use crate::net::{Close, Command, ConnKey, Driver, Handler, Io, RanOut, Read as StreamRead, Seen, Sender};
use crate::{Error, server, tls};

/// Where the daemon is: its address, its key's SHA-256, and what this home knows of it from its
/// connections before ([`tls::Known`]), which its handshake and its waits start from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remote {
    pub address: SocketAddr,
    pub daemon: Fingerprint,
    pub known: tls::Known,
}

/// A daemon's handshake that has not ended, asked of its caller as each probe goes (RFC 9002
/// §6.2): the datagrams sent in all, and how long it has waited. Its caller says whether it
/// waits on: a handshake has no deadline but its caller's, one with a daemon reached before
/// running out past the slowest seen from it and going on as one never reached does
/// (`net::RanOut::Handshake`). What its user is told of it is a stall's ([`Stall::Unreached`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unanswered {
    pub probes: u64,
    pub waited: std::time::Duration,
}

/// Whether a handshake that has not ended waits on ([`Unanswered`]).
type Waits<'a> = Box<dyn FnMut(Unanswered) -> bool + 'a>;

/// A stall of the daemon's as a command waits on its answer (R04), each told once: unheard past
/// the window its path has shown, the command waiting on with no window for as long as its
/// caller does, the daemon busy, its host loaded, or gone (ROOT's ruling A); then answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// Unheard `unheard` long, past `window` (`net::RanOut::Stalled`).
    Unheard {
        unheard: std::time::Duration,
        window: std::time::Duration,
    },
    /// Answered at last, `after` the send it stalled on (`net::RanOut::Answered`).
    Answered { after: std::time::Duration },
    /// Its handshake not ended `waited` since it began, unheard past what its path has shown
    /// ([`Told`]): `window`, the slowest handshake seen from it (`net::RanOut::Handshake`), or
    /// none on a path with no window, never reached or marked, told past the probe timeout
    /// RFC 9002 gives a path with no RTT sample (`tls::Known::COLD_PTO`). Told once a
    /// handshake; it goes on for as long as its caller waits.
    Unreached {
        waited: std::time::Duration,
        window: Option<std::time::Duration>,
    },
}

/// Whether a handshake that has not ended has been told its caller as a stall (R04), as an
/// established connection's stall is, once, past the window its path has shown (D180, ROOT's
/// ruling A). A probe within it is no stall: a loaded host's sub-millisecond probe timeout
/// told every probe, "4 probes over 424.6µs" on a command's stderr (ROOT's KVM run at
/// 21a65b459).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Told {
    /// Nothing told of it yet.
    Not,
    /// Told once ([`Stall::Unreached`]): nothing more of it is.
    Once,
}

impl Told {
    /// A probe of a handshake on a path known as `known`, `waited` since it began: the stall to
    /// tell, where its path has no window (never reached, or marked, `tls::Known::handshake_window`)
    /// and its probes have gone past the probe timeout a path with no RTT sample has
    /// (`tls::Known::COLD_PTO`), none told yet. A path with a window is told at its run-out
    /// ([`Told::ran_out`]).
    fn probe(&mut self, known: tls::Known, waited: std::time::Duration) -> Option<Stall> {
        match self {
            Told::Once => None,
            Told::Not if known.handshake_window().is_some() || waited < tls::Known::COLD_PTO => None,
            Told::Not => {
                *self = Told::Once;
                Some(Stall::Unreached { waited, window: None })
            }
        }
    }

    /// Its handshake ran out of its path's window, `waited` since it began
    /// (`RanOut::Handshake`): the stall to tell, none told yet.
    fn ran_out(&mut self, waited: std::time::Duration, window: std::time::Duration) -> Option<Stall> {
        match self {
            Told::Once => None,
            Told::Not => {
                *self = Told::Once;
                Some(Stall::Unreached {
                    waited,
                    window: Some(window),
                })
            }
        }
    }
}

/// What is told of a stall ([`Stall`]).
type Stalls<'a> = Box<dyn FnMut(Stall) + 'a>;

enum Ask {
    /// A command; `input`, whether its stdin is sent: a command given none reads the end of
    /// its input at once.
    Exec {
        request: Request,
        input: bool,
    },
    Join {
        name: String,
        secret: [u8; 32],
    },
}

enum Event {
    Connect,
    Stdin(Vec<u8>),
    StdinEnd,
    Drained,
    /// The client's terminal is this size now, rows then columns.
    Resize(u16, u16),
    /// The client received the signal of this Linux number.
    Signal(u32),
    /// What the command printed could not be written here, for this reason: it ends, its
    /// output having nowhere to go.
    Unwritten(String),
    /// Its client ends it ([`Controls::hang_up`]).
    HangUp,
}

/// What steers a command [`exec`] runs, from another thread while it runs: its terminal's
/// size, and the signals its client receives.
#[derive(Debug)]
pub struct Controls(Sender<Event>);

impl Controls {
    /// The client's terminal is `rows` by `cols` now.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), Error> {
        self.0.send(Command::Handler(Event::Resize(rows, cols)))
    }

    /// The client received the signal Linux numbers `linux`.
    pub fn signal(&self, linux: u32) -> Result<(), Error> {
        self.0.send(Command::Handler(Event::Signal(linux)))
    }

    /// Ends the command's connection, as a client gone ends it: the command there is hung
    /// up, and [`exec`] returns.
    pub fn hang_up(&self) -> Result<(), Error> {
        self.0.send(Command::Handler(Event::HangUp))
    }
}

/// What ends a command [`exec_with`] runs, from another thread, from the moment its connection
/// is made: before its daemon answers too, whose handshake has no deadline of its own
/// ([`Unanswered`]), so that only its holder's own end, a daemon's stop, ends that wait.
#[derive(Debug)]
pub struct HangUp(Sender<Event>);

impl HangUp {
    /// Ends the command's connection, as [`Controls::hang_up`] does, whether or not its
    /// handshake has ended: [`exec_with`] returns.
    pub fn now(&self) -> Result<(), Error> {
        self.0.send(Command::Handler(Event::HangUp))
    }
}

/// What the daemon answered.
enum Answer {
    Exit(i32),
    /// Refused before any command of it started: nothing of it ran.
    Refused(String),
    /// Its command started, and the daemon ended its stream before its end ([`Tag::Cut`]):
    /// it may have run.
    Cut(String),
    Joined,
}

/// What is given what steers a command once it is asked for, and the driver's sender it
/// steers through.
type Steering<'a> = (Box<dyn FnOnce(Controls) + 'a>, Sender<Event>);

struct Client<'a> {
    remote: Remote,
    identity: Option<hyper_quic::ClientConfig>,
    ask: Option<Ask>,
    /// What it asks is a join.
    joining: bool,
    conn: Option<ConnKey>,
    stream: Option<StreamId>,
    pending: Vec<u8>,
    inbound: Vec<u8>,
    stdout: Option<mpsc::SyncSender<Vec<u8>>>,
    stderr: Option<mpsc::SyncSender<Vec<u8>>>,
    /// A frame for stdout or stderr its writer had no room for yet.
    held: Option<(Tag, Vec<u8>)>,
    credit: Option<mpsc::SyncSender<()>>,
    starved: bool,
    /// The daemon reads no more of what is sent (STOP_SENDING): the rest of stdin goes
    /// unsent, as the command it was for has ended.
    unread: bool,
    /// The daemon finished its side: all it sent is read.
    ended: bool,
    /// The command's control stream, once something has steered it.
    control: Option<StreamId>,
    /// Frames for the control stream not yet written.
    controls: Vec<u8>,
    /// The daemon reads no more of the control stream: what would steer the command goes
    /// nowhere.
    uncontrolled: bool,
    /// Given what steers the command once it is asked for: before then, there is no command
    /// for a signal to reach, which its caller acts on itself.
    steering: Option<Steering<'a>>,
    /// Given what the command tells of the microVM it made ([`Tag::Made`]).
    made: Option<Made<'a>>,
    /// Asked, as each probe of a handshake that has not ended goes, whether it waits on.
    waits: Option<Waits<'a>>,
    /// Told of each stall of its daemon's.
    stalls: Option<Stalls<'a>>,
    /// Whether its handshake has been told as a stall.
    told: Told,
    answer: Option<Answer>,
    failed: Option<String>,
    done: bool,
    /// What its connection saw of the path, as it ended.
    seen: Seen,
}

impl Client<'_> {
    /// Tells its caller of a stall ([`Stall`]).
    fn stall(&mut self, stall: Stall) {
        if let Some(stalls) = self.stalls.as_mut() {
            stalls(stall);
        }
    }

    /// What its connection saw, kept before it goes.
    fn note(&mut self, io: &mut Io<'_>) {
        if let Some(seen) = self.conn.and_then(|conn| io.seen(conn)) {
            self.seen = seen;
        }
    }

    fn flush(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        while !self.pending.is_empty() {
            match io.write(conn, stream, &self.pending) {
                Ok(0) => break,
                Ok(n) => {
                    self.pending.drain(..n.min(self.pending.len()));
                }
                // The daemon reads no more of it: its answer is still to come.
                Err(_) => {
                    self.stop_input();
                    return;
                }
            }
        }
        // Its reader reads on once the transport has taken all it read: the daemon's flow
        // control (RFC 9000 §4.1) is stdin's only read-ahead.
        if self.starved && self.pending.is_empty() {
            self.starved = false;
            if let Some(credit) = &self.credit {
                let _ = credit.try_send(());
            }
        }
    }

    /// Writes what steers the command to its control stream, opened once the command's own is.
    fn flush_controls(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        if self.controls.is_empty() || self.uncontrolled {
            return;
        }
        let control = match self.control {
            Some(control) => control,
            None => {
                let Ok(control) = io.open(conn) else {
                    self.uncontrolled = true;
                    self.controls.clear();
                    return;
                };
                let mut header = Vec::new();
                if frame::put(&mut header, Tag::Controls, &u64::from(stream).to_be_bytes()).is_err() {
                    return;
                }
                self.controls.splice(0..0, header);
                self.control = Some(control);
                control
            }
        };
        while !self.controls.is_empty() {
            match io.write(conn, control, &self.controls) {
                Ok(0) => break,
                Ok(n) => {
                    self.controls.drain(..n.min(self.controls.len()));
                }
                Err(_) => {
                    self.uncontrolled = true;
                    self.controls.clear();
                }
            }
        }
    }

    /// Queues `tag`'s frame to steer the command, unless it is past steering.
    fn steer(&mut self, io: &mut Io<'_>, tag: Tag, payload: &[u8]) {
        if self.uncontrolled || self.done || frame::put(&mut self.controls, tag, payload).is_err() {
            return;
        }
        self.flush_controls(io);
    }

    /// What is left of stdin goes unsent: the daemon reads no more of it.
    fn stop_input(&mut self) {
        self.unread = true;
        self.pending.clear();
        // The reader of stdin waits for credit no more, and ends.
        self.credit = None;
    }

    fn fail(&mut self, io: &mut Io<'_>, why: String) {
        if self.failed.is_none() && self.answer.is_none() {
            self.failed = Some(why);
        }
        self.note(io);
        if let Some(conn) = self.conn {
            io.close(conn, 0, "");
        }
        self.done = true;
    }

    fn answered(&mut self, io: &mut Io<'_>, answer: Answer) {
        self.answer = Some(answer);
        self.note(io);
        if let Some(conn) = self.conn {
            io.close(conn, 0, "");
        }
        self.done = true;
    }

    /// Takes what the stream has, until a writer has no room: each whole frame before reading
    /// more, so that the stream's end is judged after all it carried.
    fn take(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        loop {
            if !self.deliver_held() {
                return;
            }
            match frame::take(&mut self.inbound) {
                Ok(Some((tag, payload))) => match tag {
                    Tag::Stdout | Tag::Stderr => {
                        self.held = Some((tag, payload));
                        continue;
                    }
                    Tag::Exit => {
                        let status = payload
                            .get(..4)
                            .and_then(|b| <[u8; 4]>::try_from(b).ok())
                            .map_or(1, i32::from_be_bytes);
                        self.answered(io, Answer::Exit(status));
                        return;
                    }
                    Tag::Refused => {
                        self.answered(
                            io,
                            Answer::Refused(String::from_utf8_lossy(&payload).into_owned()),
                        );
                        return;
                    }
                    Tag::Cut => {
                        self.answered(io, Answer::Cut(String::from_utf8_lossy(&payload).into_owned()));
                        return;
                    }
                    Tag::Joined => {
                        self.answered(io, Answer::Joined);
                        return;
                    }
                    Tag::Made => {
                        if let Some(made) = &mut self.made {
                            made(&payload);
                        }
                        continue;
                    }
                    other => {
                        self.fail(io, format!("the daemon sent a client's frame ({other:?})"));
                        return;
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    self.fail(io, format!("the daemon's answer: {e}"));
                    return;
                }
            }
            if self.ended {
                self.fail(io, "the daemon ended the command's stream without its end".into());
                return;
            }
            match io.read(conn, stream) {
                StreamRead::Bytes(bytes) => self.inbound.extend_from_slice(&bytes),
                StreamRead::End(bytes) => {
                    self.inbound.extend_from_slice(&bytes);
                    self.ended = true;
                }
                StreamRead::Blocked => return,
                StreamRead::Reset => {
                    self.fail(io, "the daemon reset the command's stream".into());
                    return;
                }
            }
        }
    }

    /// Gives the frame held back to its writer; whether there is room for more.
    fn deliver_held(&mut self) -> bool {
        let Some((tag, payload)) = self.held.take() else {
            return true;
        };
        let writer = if tag == Tag::Stdout {
            &self.stdout
        } else {
            &self.stderr
        };
        let Some(writer) = writer else {
            return true;
        };
        match writer.try_send(payload) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(payload)) => {
                self.held = Some((tag, payload));
                false
            }
            // The writer is gone: what it would have written goes nowhere.
            Err(mpsc::TrySendError::Disconnected(_)) => true,
        }
    }
}

impl Handler for Client<'_> {
    type Command = Event;

    /// The daemon's key is the one pinned: TLS refused any other. A connection it fails on,
    /// it closes itself.
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        let stream = match io.open(conn) {
            Ok(stream) => stream,
            Err(e) => {
                self.fail(io, e.to_string());
                return Ok(());
            }
        };
        self.stream = Some(stream);
        let framed = match self.ask.take() {
            Some(Ask::Exec { request, input }) => request.encode().and_then(|p| {
                let mut out = Vec::new();
                frame::put(&mut out, Tag::Exec, &p)?;
                if !input {
                    frame::put(&mut out, Tag::StdinEnd, &[])?;
                }
                Ok(out)
            }),
            Some(Ask::Join { name, secret }) => {
                io.export(conn, invite::EXPORTER_LABEL).and_then(|exported| {
                    let proof = invite::proof(&secret, &exported);
                    let mut payload = name.into_bytes();
                    payload.push(0);
                    payload.extend_from_slice(&proof);
                    let mut out = Vec::new();
                    frame::put(&mut out, Tag::Join, &payload).map(|()| out)
                })
            }
            None => Err(Error::new("nothing to ask")),
        };
        match framed {
            Ok(framed) => {
                self.pending.splice(0..0, framed);
                self.flush(io);
                // The command is asked for: what steers it reaches it from now on.
                if let Some((steered, sender)) = self.steering.take() {
                    steered(Controls(sender));
                }
                self.flush_controls(io);
            }
            Err(e) => self.fail(io, e.to_string()),
        }
        Ok(())
    }

    /// None comes: its transport gives the daemon no stream to open (`tls`).
    fn opened(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if self.control == Some(stream) {
            // The daemon answers nothing on it: its end, read to let it go.
            while let StreamRead::Bytes(_) = io.read(conn, stream) {}
            return;
        }
        self.take(io);
    }

    fn writable(&mut self, io: &mut Io<'_>, _conn: ConnKey, stream: StreamId) {
        if self.control == Some(stream) {
            self.flush_controls(io);
        } else {
            self.flush(io);
        }
    }

    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, stream: StreamId) {
        if self.control == Some(stream) {
            self.uncontrolled = true;
            self.controls.clear();
        } else {
            self.stop_input();
        }
    }

    fn lost(&mut self, io: &mut Io<'_>, _conn: ConnKey, why: &str, code: Option<u64>) {
        if !self.done {
            self.note(io);
        }
        if self.answer.is_none() && self.failed.is_none() && code == Some(u64::from(server::UNENROLLED)) {
            // The daemon serves no client it has not enrolled now: refused before a stream
            // could say so, in the words the stream would have said.
            let words = if self.joining {
                server::NOT_INVITED
            } else {
                server::NOT_ENROLLED
            };
            self.answer = Some(Answer::Refused(words.to_string()));
        }
        if self.answer.is_none() && self.failed.is_none() {
            let at = self.remote.address;
            self.failed = Some(match self.stream {
                Some(_) => format!("the connection to {at} ended: {why}"),
                None => format!(
                    "no shards daemon answered at {at} ({why}): is it listening there (shards run daemon --listen ADDR:PORT)?"
                ),
            });
        }
        self.done = true;
    }

    fn unanswered(&mut self, io: &mut Io<'_>, _conn: ConnKey, probes: u64, waited: std::time::Duration) {
        if let Some(stall) = self.told.probe(self.remote.known, waited) {
            self.stall(stall);
        }
        let asked = Unanswered { probes, waited };
        let goes_on = self.waits.as_mut().is_none_or(|waits| waits(asked));
        if !goes_on {
            self.fail(
                io,
                format!(
                    "no shards daemon answered at {}: {probes} probes over {waited:?}",
                    self.remote.address
                ),
            );
        }
    }

    /// Its daemon unheard past its window, and answered after, or its handshake past its
    /// path's window: told its caller ([`Stall`]).
    fn ran_out(&mut self, _io: &mut Io<'_>, _conn: ConnKey, what: RanOut) {
        let stall = match what {
            RanOut::Stalled { unheard, window } => Stall::Unheard { unheard, window },
            RanOut::Answered { after } => Stall::Answered { after },
            RanOut::Handshake { waited, window } => match self.told.ran_out(waited, window) {
                Some(stall) => stall,
                None => return,
            },
            RanOut::Send { .. } => return,
        };
        self.stall(stall);
    }

    fn command(&mut self, io: &mut Io<'_>, command: Event) {
        match command {
            Event::Connect => {
                // Hung up before its connection was made ([`HangUp`]): none is.
                let Some(config) = self.identity.take().filter(|_| !self.done) else {
                    return;
                };
                match io.connect(self.remote.address, config) {
                    Ok(conn) => self.conn = Some(conn),
                    Err(e) => self.fail(io, e.to_string()),
                }
            }
            Event::Stdin(_) | Event::StdinEnd if self.unread => {}
            Event::Stdin(bytes) => {
                if frame::put(&mut self.pending, Tag::Stdin, &bytes).is_ok() {
                    self.starved = true;
                    self.flush(io);
                }
            }
            Event::StdinEnd => {
                if frame::put(&mut self.pending, Tag::StdinEnd, &[]).is_ok() {
                    self.flush(io);
                }
            }
            Event::Drained => self.take(io),
            Event::Resize(rows, cols) => self.steer(io, Tag::Resize, &frame::size(rows, cols)),
            Event::Signal(linux) => self.steer(io, Tag::Signal, &linux.to_be_bytes()),
            // Closed, the connection hangs the command up there, as a client gone would.
            Event::Unwritten(why) => self.fail(io, format!("writing the command's output: {why}")),
            Event::HangUp if !self.done => self.fail(io, "hung up by its client".into()),
            Event::HangUp => {}
        }
    }

    fn done(&self) -> bool {
        self.done
    }
}

/// A writer of the command's output: what the driver gives it, written in order, and the
/// driver told when there is room again.
fn writer<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    mut out: impl Write + Send + 'static,
    sender: Sender<Event>,
) -> Result<mpsc::SyncSender<Vec<u8>>, Error> {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(server::QUEUED_CHUNKS);
    std::thread::Builder::new()
        .name("remote-out".into())
        .spawn_scoped(scope, move || {
            while let Ok(bytes) = rx.recv() {
                if let Err(e) = out.write_all(&bytes).and_then(|()| out.flush()) {
                    let _ = sender.send(Command::Handler(Event::Unwritten(e.to_string())));
                    break;
                }
                let _ = sender.send(Command::Handler(Event::Drained));
            }
        })
        .map_err(|e| Error::new(format!("a thread for the command's output: {e}")))?;
    Ok(tx)
}

/// Runs `request` on the daemon `remote`, as the home whose identity is `identity`: `stdin`,
/// where given, sent as it comes, and what the command writes to its stdout and stderr
/// written to `stdout` and `stderr`; `steered` is given what steers it once it is asked for,
/// and not before: a signal before then has no command to reach. Its exit status, or, for a
/// request the daemon refused, the daemon's reason.
pub fn exec(
    identity: &Identity,
    remote: Remote,
    request: Request,
    stdin: Option<Box<dyn Read + Send>>,
    stdout: impl Write + Send + 'static,
    stderr: impl Write + Send + 'static,
    steered: impl FnOnce(Controls),
) -> Result<Result<i32, String>, Error> {
    exec_made(identity, remote, request, stdin, stdout, stderr, steered, |_| {})
}

/// [`exec`], and `made` given what the command tells of the microVM it made, its ID, as it
/// comes ([`Tag::Made`]): on this thread, before the command's exit status.
#[allow(clippy::too_many_arguments)]
pub fn exec_made(
    identity: &Identity,
    remote: Remote,
    request: Request,
    stdin: Option<Box<dyn Read + Send>>,
    stdout: impl Write + Send + 'static,
    stderr: impl Write + Send + 'static,
    steered: impl FnOnce(Controls),
    made: impl FnMut(&[u8]),
) -> Result<Result<i32, String>, Error> {
    let input = stdin.is_some();
    let mut seen = Seen::default();
    let answer = run(
        identity,
        remote,
        Ask::Exec { request, input },
        stdin,
        Some((Box::new(stdout), Box::new(stderr))),
        Hooks {
            steered: Some(Box::new(steered)),
            made: Some(Box::new(made)),
            ..Hooks::default()
        },
        &mut seen,
    )?;
    exit_of(answer)
}

/// [`exec_made`], `unanswered` asked as each probe of a handshake that has not ended goes
/// whether it waits on ([`Unanswered`]: a handshake is waited for as long as its caller does,
/// one with a daemon reached before past its run-out too), `stalled` told of each stall of its
/// daemon's and its end, its handshake's once past what its path has shown ([`Stall`]: waited
/// through for as long as its caller waits), `hang_up` given what
/// ends it from another thread once its connection is made ([`HangUp`]), and what its connection
/// saw ([`Seen`]), answered or not, for what its caller keeps of the daemon
/// ([`tls::Known::after`]).
#[allow(clippy::too_many_arguments)]
pub fn exec_with(
    identity: &Identity,
    remote: Remote,
    request: Request,
    stdin: Option<Box<dyn Read + Send>>,
    stdout: impl Write + Send + 'static,
    stderr: impl Write + Send + 'static,
    steered: impl FnOnce(Controls),
    made: impl FnMut(&[u8]),
    unanswered: impl FnMut(Unanswered) -> bool,
    stalled: impl FnMut(Stall),
    hang_up: impl FnOnce(HangUp),
) -> (Result<Result<i32, String>, Error>, Seen) {
    let input = stdin.is_some();
    let mut seen = Seen::default();
    let answer = run(
        identity,
        remote,
        Ask::Exec { request, input },
        stdin,
        Some((Box::new(stdout), Box::new(stderr))),
        Hooks {
            steered: Some(Box::new(steered)),
            made: Some(Box::new(made)),
            waits: Some(Box::new(unanswered)),
            stalls: Some(Box::new(stalled)),
            hang_up: Some(Box::new(hang_up)),
        },
        &mut seen,
    );
    (answer.and_then(exit_of), seen)
}

/// A command's end: its status, its refusal before it ran, or, cut after it started, no
/// answer, as a connection lost then is: it may have run (D176 part 6).
fn exit_of(answer: Answer) -> Result<Result<i32, String>, Error> {
    match answer {
        Answer::Exit(status) => Ok(Ok(status)),
        Answer::Refused(why) => Ok(Err(why)),
        Answer::Cut(why) => Err(Error::new(format!(
            "{why}, after the command started: it may have run"
        ))),
        Answer::Joined => Err(Error::new("the daemon answered a command as a join")),
    }
}

/// Joins the daemon `invitation` names, as the home whose identity is `identity`: Ok once
/// the daemon has enrolled it, or the daemon's reason for refusing; and what its connection
/// saw ([`Seen`]), the first of the daemon's path its home keeps ([`tls::Known::after`]).
pub fn join(identity: &Identity, invitation: &Invitation) -> (Result<Result<(), String>, Error>, Seen) {
    // A daemon joined is one this home has never reached.
    let remote = Remote {
        address: invitation.address,
        daemon: invitation.daemon,
        known: tls::Known::Never,
    };
    let ask = Ask::Join {
        name: invitation.name.clone(),
        secret: invitation.secret,
    };
    let mut seen = Seen::default();
    let joined =
        run(identity, remote, ask, None, None, Hooks::default(), &mut seen).and_then(|answer| match answer {
            Answer::Joined => Ok(Ok(())),
            Answer::Refused(why) => Ok(Err(why)),
            Answer::Exit(_) | Answer::Cut(_) => Err(Error::new("the daemon answered a join as a command")),
        });
    (joined, seen)
}

type Outputs = (Box<dyn Write + Send>, Box<dyn Write + Send>);

/// What is given what a command tells of the microVM it made ([`exec_made`]).
type Made<'a> = Box<dyn FnMut(&[u8]) + 'a>;

/// What a run's caller is given and asked as it goes, besides its output: what steers the
/// command once it is asked for, what it tells of the microVM it made, whether a handshake
/// that has not ended waits on, each stall of its daemon's, and what ends it from another
/// thread from the start.
#[derive(Default)]
struct Hooks<'a> {
    steered: Option<Box<dyn FnOnce(Controls) + 'a>>,
    made: Option<Made<'a>>,
    waits: Option<Waits<'a>>,
    stalls: Option<Stalls<'a>>,
    hang_up: Option<Box<dyn FnOnce(HangUp) + 'a>>,
}

/// Asks `ask` of `remote`; `seen` given what its connection saw, once it has ended.
fn run(
    identity: &Identity,
    remote: Remote,
    ask: Ask,
    stdin: Option<Box<dyn Read + Send>>,
    outputs: Option<Outputs>,
    hooks: Hooks<'_>,
    seen: &mut Seen,
) -> Result<Answer, Error> {
    let Hooks {
        steered,
        made,
        waits,
        stalls,
        hang_up,
    } = hooks;
    let config = tls::client(identity, remote.daemon, remote.known)?;
    let any: SocketAddr = if remote.address.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let joining = matches!(ask, Ask::Join { .. });
    std::thread::scope(|scope| {
        let client = Client {
            remote,
            identity: Some(config),
            ask: Some(ask),
            joining,
            conn: None,
            stream: None,
            pending: Vec::new(),
            inbound: Vec::new(),
            stdout: None,
            stderr: None,
            held: None,
            credit: None,
            starved: false,
            unread: false,
            ended: false,
            control: None,
            controls: Vec::new(),
            uncontrolled: false,
            steering: None,
            made,
            waits,
            stalls,
            told: Told::Not,
            answer: None,
            failed: None,
            done: false,
            seen: Seen::default(),
        };
        let (mut driver, sender) = Driver::bind(any, None, client, remote.known)?;
        // Its one peer: a refusal of its host's (ICMP) reads as one.
        driver.aim(remote.address)?;
        if let Some((stdout, stderr)) = outputs {
            let handler = &mut driver_handler(&mut driver);
            handler.stdout = Some(writer(scope, stdout, sender.try_clone()?)?);
            handler.stderr = Some(writer(scope, stderr, sender.try_clone()?)?);
        }
        if let Some(mut stdin) = stdin {
            let (credit_tx, credit_rx) = mpsc::sync_channel::<()>(1);
            let _ = credit_tx.try_send(());
            driver_handler(&mut driver).credit = Some(credit_tx);
            let to = sender.try_clone()?;
            // Reading stdin may block past the command's end: the thread is not waited for.
            std::thread::Builder::new()
                .name("remote-in".into())
                .spawn(move || {
                    let mut buffer = vec![0u8; server::READ_MOST];
                    while credit_rx.recv().is_ok() {
                        match stdin.read(&mut buffer) {
                            Ok(0) | Err(_) => {
                                let _ = to.send(Command::Handler(Event::StdinEnd));
                                return;
                            }
                            Ok(n) => {
                                let bytes = buffer.get(..n).unwrap_or_default().to_vec();
                                if to.send(Command::Handler(Event::Stdin(bytes))).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                })
                .map_err(|e| Error::new(format!("a thread for the command's input: {e}")))?;
        }
        if let Some(steered) = steered {
            driver_handler(&mut driver).steering = Some((steered, sender.try_clone()?));
        }
        if let Some(hang_up) = hang_up {
            hang_up(HangUp(sender.try_clone()?));
        }
        sender.send(Command::Handler(Event::Connect))?;
        driver.run()?;
        let mut client = driver.into_handler();
        *seen = client.seen;
        // The writers finish what they were given, and end.
        client.stdout = None;
        client.stderr = None;
        if let Some(why) = client.failed {
            return Err(Error::new(why));
        }
        client
            .answer
            .ok_or_else(|| Error::new(format!("the connection to {} ended unanswered", remote.address)))
    })
}

fn driver_handler<'d, 'a>(driver: &'d mut Driver<Client<'a>>) -> &'d mut Client<'a> {
    driver.handler_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command refused before it started is its refusal; one cut after it started is no
    /// answer, as a lost connection is, for it may have run; its exit is its status (D176
    /// part 6).
    #[test]
    fn a_cut_command_is_unanswered_and_a_refused_one_refused() {
        assert_eq!(exit_of(Answer::Exit(3)).unwrap(), Ok(3));
        assert_eq!(
            exit_of(Answer::Refused("not enrolled".into())).unwrap(),
            Err("not enrolled".to_string())
        );
        let cut = exit_of(Answer::Cut("a frame out of place (Joined)".into())).unwrap_err();
        assert_eq!(
            cut.to_string(),
            "a frame out of place (Joined), after the command started: it may have run"
        );
    }

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    fn us(n: u64) -> std::time::Duration {
        std::time::Duration::from_micros(n)
    }

    /// A daemon reached before, whose slowest handshake was 1.7 ms: its handshake's probes
    /// within that window are told nothing, however many go (ROOT's KVM run at 21a65b459 on a
    /// loaded adacachy2: "shards: gpu: no answer yet from 127.0.0.1:44476: 4 probes over
    /// 424.6µs" on a command's stderr); its run-out past the window is told once, and nothing
    /// after it, its probes going on (D180, ROOT's ruling A).
    #[test]
    fn a_handshake_within_its_paths_window_is_told_nothing_and_once_past_it() {
        let known = tls::Known::Reached(tls::Path {
            rtt: us(60),
            handshake: us(1700),
            answer: us(900),
            lost: None,
        });
        let mut told = Told::Not;
        for waited in [us(110), us(220), us(424), us(900)] {
            assert_eq!(told.probe(known, waited), None, "a probe {waited:?} in");
        }
        assert_eq!(
            told.ran_out(ms(31), us(1700)),
            Some(Stall::Unreached {
                waited: ms(31),
                window: Some(us(1700))
            })
        );
        for waited in [ms(40), ms(80), ms(2000)] {
            assert_eq!(
                told.probe(known, waited),
                None,
                "a probe {waited:?} in, past its run-out"
            );
        }
        assert_eq!(told.ran_out(ms(90), us(1700)), None);
    }

    /// A path with no window, never reached or whose last wait ran out: its handshake is told
    /// once its probes have gone past the probe timeout RFC 9002 gives a path with no RTT
    /// sample (`tls::Known::COLD_PTO`), and once only; a marked path's own short RTT, which
    /// its probes start from, tells nothing sooner.
    #[test]
    fn a_handshake_with_no_window_is_told_once_past_the_cold_probe_timeout() {
        let marked = tls::Known::RanOut(tls::Path {
            rtt: us(60),
            handshake: us(1700),
            answer: us(900),
            lost: None,
        });
        for known in [tls::Known::Never, marked] {
            let mut told = Told::Not;
            for waited in [us(424), ms(30), ms(500), ms(998)] {
                assert_eq!(
                    told.probe(known, waited),
                    None,
                    "{known:?}: a probe {waited:?} in"
                );
            }
            assert_eq!(
                told.probe(known, ms(1000)),
                Some(Stall::Unreached {
                    waited: ms(1000),
                    window: None
                }),
                "{known:?}"
            );
            for waited in [ms(2000), ms(4000)] {
                assert_eq!(
                    told.probe(known, waited),
                    None,
                    "{known:?}: a probe {waited:?} in"
                );
            }
        }
    }
}
