//! The daemon's half (D180): the clients it enrolled run commands, each on a stream of its own,
//! and a client that is not enrolled may only join by an invitation. What a client asks of the
//! machine is its [`Host`]'s to judge and run.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Instant;

use hyper_quic::{StreamId, VarInt};

use crate::frame::{self, Request, Tag};
use crate::identity::Identity;
use crate::invite;
use crate::net::{Close, Command, ConnKey, Driver, Handler, Io, Read as StreamRead, Sender};
use crate::{Error, tls};

/// How many bytes of a command's output wait unsent before its readers wait too.
const HIGH_WATER: usize = 1 << 20;

/// A command a client asked for, started: its stdio, and a wait for its exit status.
#[allow(missing_debug_implementations)]
pub struct Running {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
    /// Waits for the command to end; its status, or 128 plus the signal that ended it.
    pub wait: Box<dyn FnOnce() -> i32 + Send>,
    /// Hangs the command up, as a terminal that went away would (SIGHUP).
    pub hangup: Box<dyn Fn() + Send>,
    /// Sizes the command's terminal, rows then columns: none, for a command on no terminal.
    pub resize: Option<Box<dyn Fn(u16, u16) + Send>>,
    /// Signals the command as its client was, by the signal's Linux number.
    pub signal: Box<dyn Fn(u32) + Send>,
    /// What the command tells its client of the microVM it made, read to its end: its ID
    /// ([`Tag::Made`]), where the client asked for it.
    pub made: Option<Box<dyn Read + Send>>,
}

/// The most a command's [`Running::made`] says: an ID, 64 hex digits.
const MADE_MOST: u64 = 4 << 10;

/// The machine a daemon serves its clients on.
pub trait Host: Send {
    /// The name of the enrolled client whose key's SubjectPublicKeyInfo is `spki`, if one is.
    fn enrolled(&self, spki: &[u8]) -> Option<String>;
    /// Redeems invitation `name`, whose secret `proof` must prove over the connection whose
    /// exporter gave `exported`, enrolling the client `spki` under it.
    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error>;
    /// Starts `request`'s command for the enrolled client `client`.
    fn start(&self, client: &str, request: Request) -> Result<Running, Error>;
    /// How many invitations the machine holds open: as many clients it has not enrolled may
    /// be connected at once, to join, as an invitation enrolls one client.
    fn invited(&self) -> usize;
}

/// The daemon's answer to any join that failed, whatever made it fail: a refusal tells
/// nothing of the invitations it holds.
pub const NOT_INVITED: &str = "this invitation is not one this machine holds";

/// The daemon's answer to a command from a client it has not enrolled.
pub const NOT_ENROLLED: &str = "this machine has not enrolled you: join it by an invitation first";

/// The code a daemon closes a connection with whose peer it has not enrolled and does not
/// serve: no invitation of its is free for one, or the peer's time to join is up. Its client
/// says what the refusal on a stream would have said.
pub const UNENROLLED: u32 = 2;

enum Event {
    Output {
        conn: ConnKey,
        stream: StreamId,
        tag: Tag,
        bytes: Vec<u8>,
    },
    OutputEnd {
        conn: ConnKey,
        stream: StreamId,
        tag: Tag,
    },
    Exited {
        conn: ConnKey,
        stream: StreamId,
        status: i32,
    },
    /// The command's stdin took what it was given, or takes no more: the stream is read on.
    InputTaken { conn: ConnKey, stream: StreamId },
}

#[derive(Default)]
struct Stream {
    inbound: Vec<u8>,
    pending: Vec<u8>,
    /// The command's stdin, until its end, which this sender's drop is.
    stdin: Option<mpsc::SyncSender<Vec<u8>>>,
    /// Bytes for stdin it had no room for yet: the stream is read no more until it has, and
    /// its flow control holds the client back meanwhile.
    held: Option<Vec<u8>>,
    /// The client sent the end of stdin: it follows what is held.
    stdin_ends: bool,
    credits: Vec<mpsc::SyncSender<()>>,
    starved: bool,
    stdout_done: bool,
    stderr_done: bool,
    /// What the command told of the microVM it made is queued, or it tells nothing.
    made_done: bool,
    status: Option<i32>,
    hangup: Option<Box<dyn Fn() + Send>>,
    /// The request frame came.
    asked: bool,
    /// The client finished its side: all it sent is read.
    ended: bool,
    /// The stream's last frame is queued: its exit, refusal or join.
    last: bool,
    /// The last frame is written, and this side finished.
    finished: bool,
    /// The client left the stream, resetting it or reading no more of it: nothing more goes
    /// either way.
    gone: bool,
    /// The command stream this one steers, for a control stream.
    steers: Option<StreamId>,
    /// The command's terminal's size, as its client's changes.
    resize: Option<Box<dyn Fn(u16, u16) + Send>>,
    /// The command's signals, as its client's.
    signal: Option<Box<dyn Fn(u32) + Send>>,
}

impl Stream {
    /// Whether its command started and has not ended.
    fn running(&self) -> bool {
        self.hangup.is_some() && self.status.is_none()
    }

    fn flush(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        while !self.pending.is_empty() {
            match io.write(conn, id, &self.pending) {
                Ok(0) => break,
                Ok(n) => {
                    self.pending.drain(..n.min(self.pending.len()));
                }
                // The client reads no more of it.
                Err(_) => {
                    self.gone = true;
                    return;
                }
            }
        }
        if self.starved && self.pending.len() < HIGH_WATER {
            self.starved = false;
            for credit in &self.credits {
                let _ = credit.try_send(());
            }
        }
        if self.pending.is_empty() && self.last && !self.finished {
            self.finished = true;
            let _ = io.finish(conn, id);
        }
    }

    /// Queues `tag`'s frame as the stream's last. A command still running is hung up, as its
    /// client hears no more of it, and what the client still sends goes unread: it is asked
    /// to stop (STOP_SENDING), as an HTTP/3 server that needs no more of a request's body
    /// does (RFC 9114 §4.1.2).
    fn end(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId, tag: Tag, payload: &[u8]) {
        if self.last {
            return;
        }
        if frame::put(&mut self.pending, tag, payload).is_err() {
            let _ = frame::put(&mut self.pending, Tag::Refused, b"an answer too long");
        }
        self.last = true;
        if self.running()
            && let Some(hangup) = &self.hangup
        {
            hangup();
        }
        self.stdin = None;
        self.held = None;
        if !self.ended {
            io.stop(conn, id);
        }
        self.flush(io, conn, id);
    }

    /// Queues the exit status once the command has ended and all it wrote is queued.
    fn exit_when_done(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        if let Some(status) = self.status
            && self.stdout_done
            && self.stderr_done
            && self.made_done
        {
            self.end(io, conn, id, Tag::Exit, &status.to_be_bytes());
        } else {
            self.flush(io, conn, id);
        }
    }

    /// Gives stdin what is held for it, and then its end, once the client sent it; whether
    /// stdin had room.
    fn deliver(&mut self) -> bool {
        if let Some(bytes) = self.held.take()
            && let Some(stdin) = &self.stdin
            && let Err(mpsc::TrySendError::Full(bytes)) = stdin.try_send(bytes)
        {
            self.held = Some(bytes);
            return false;
        }
        if self.stdin_ends {
            self.stdin = None;
        }
        true
    }
}

struct Peer {
    spki: Vec<u8>,
    client: Option<String>,
    streams: HashMap<StreamId, Stream>,
    /// When its handshake was done.
    since: Instant,
}

struct Server<H: Host> {
    host: H,
    peers: HashMap<ConnKey, Peer>,
    sender: Option<Sender<Event>>,
}

/// What [`Server::frame`] found.
enum Taken {
    /// A frame, acted on.
    Frame,
    /// A frame that steers the command on the stream named: its control stream's first, or a
    /// resize or a signal.
    Steer(StreamId, Tag, Vec<u8>),
    /// No whole frame yet.
    Waiting,
}

impl<H: Host> Server<H> {
    fn stream(&mut self, conn: ConnKey, id: StreamId) -> Option<&mut Stream> {
        self.peers.get_mut(&conn).and_then(|p| p.streams.get_mut(&id))
    }

    /// The connections of peers the machine has not enrolled, and when each is due to have
    /// joined: as long after its handshake as a client waits for its daemon, the idle bound.
    fn strangers(&self) -> impl Iterator<Item = (ConnKey, Option<Instant>)> + '_ {
        self.peers
            .iter()
            .filter(|(_, p)| p.client.is_none())
            .map(|(conn, p)| (*conn, p.since.checked_add(tls::IDLE)))
    }

    /// Reads `id` and acts on its frames: each whole one before reading more, and none while
    /// its command's stdin has no room for what came before.
    fn serve(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        loop {
            let Some(stream) = self.stream(conn, id) else {
                return;
            };
            if stream.last || stream.gone || !stream.deliver() {
                break;
            }
            match self.frame(io, conn, id) {
                Taken::Frame => continue,
                Taken::Steer(target, tag, payload) => {
                    self.steer(io, conn, id, target, tag, &payload);
                    continue;
                }
                Taken::Waiting => {}
            }
            let Some(stream) = self.stream(conn, id) else {
                return;
            };
            if stream.ended {
                // All the client sent is read: its stdin ends, or a frame was cut short.
                if stream.inbound.is_empty() {
                    stream.stdin_ends = true;
                    stream.deliver();
                } else {
                    stream.end(io, conn, id, Tag::Refused, b"a frame cut short");
                }
                break;
            }
            match io.read(conn, id) {
                StreamRead::Bytes(bytes) => stream.inbound.extend_from_slice(&bytes),
                StreamRead::End(bytes) => {
                    stream.inbound.extend_from_slice(&bytes);
                    stream.ended = true;
                }
                StreamRead::Blocked => break,
                StreamRead::Reset => {
                    stream.gone = true;
                    break;
                }
            }
        }
        self.settle(io, conn, id);
    }

    /// Acts on the frame at the front of `id`'s bytes, once it has come whole. The request is
    /// judged by its header, before its bytes are waited for: a client this machine has not
    /// enrolled may only join, and is waited for no longer than a join takes.
    fn frame(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) -> Taken {
        let Some(peer) = self.peers.get_mut(&conn) else {
            return Taken::Waiting;
        };
        let Some(stream) = peer.streams.get_mut(&id) else {
            return Taken::Waiting;
        };
        if !stream.asked {
            let refusal = match frame::peek(&stream.inbound) {
                Ok(None) => return Taken::Waiting,
                Ok(Some((Tag::Exec, _))) if peer.client.is_none() => NOT_ENROLLED.to_string(),
                Ok(Some((Tag::Join, len))) if len > invite::JOIN_MAX => NOT_INVITED.to_string(),
                Ok(Some((Tag::Exec | Tag::Join, _))) => String::new(),
                Ok(Some((Tag::Controls, 8))) if peer.client.is_some() => String::new(),
                Ok(Some((other, _))) => format!("a frame out of place ({other:?})"),
                Err(e) => e.to_string(),
            };
            if !refusal.is_empty() {
                stream.end(io, conn, id, Tag::Refused, refusal.as_bytes());
                return Taken::Frame;
            }
        }
        let (tag, payload) = match frame::take(&mut stream.inbound) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Taken::Waiting,
            Err(e) => {
                stream.end(io, conn, id, Tag::Refused, e.to_string().as_bytes());
                return Taken::Frame;
            }
        };
        if let Some(target) = stream.steers {
            if let Tag::Resize | Tag::Signal = tag {
                return Taken::Steer(target, tag, payload);
            }
            let why = format!("a frame out of place ({tag:?})");
            stream.end(io, conn, id, Tag::Refused, why.as_bytes());
            return Taken::Frame;
        }
        match tag {
            Tag::Controls if !stream.asked => {
                stream.asked = true;
                let target = <[u8; 8]>::try_from(payload.as_slice())
                    .ok()
                    .and_then(|named| VarInt::from_u64(u64::from_be_bytes(named)).ok());
                match target {
                    Some(target) => return Taken::Steer(StreamId::from(target), Tag::Controls, Vec::new()),
                    None => stream.end(io, conn, id, Tag::Refused, b"a control stream for no stream"),
                }
            }
            Tag::Exec if !stream.asked => {
                stream.asked = true;
                let Some(client) = peer.client.as_deref() else {
                    stream.end(io, conn, id, Tag::Refused, NOT_ENROLLED.as_bytes());
                    return Taken::Frame;
                };
                let started = Request::decode(&payload).and_then(|request| self.host.start(client, request));
                let relayed = match (started, &self.sender) {
                    (Ok(running), Some(sender)) => relay(running, conn, id, sender),
                    (Ok(_), None) => Err(Error::new("the daemon's listener is not running")),
                    (Err(e), _) => Err(e),
                };
                match relayed {
                    Ok(relayed) => {
                        stream.stdin = Some(relayed.stdin);
                        stream.credits = relayed.credits;
                        stream.made_done = !relayed.tells;
                        stream.hangup = Some(relayed.hangup);
                        stream.resize = relayed.resize;
                        stream.signal = Some(relayed.signal);
                    }
                    Err(e) => stream.end(io, conn, id, Tag::Refused, e.to_string().as_bytes()),
                }
            }
            Tag::Join if !stream.asked => {
                stream.asked = true;
                let mut parts = payload.splitn(2, |b| *b == 0);
                let name = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                let proof = parts.next().unwrap_or_default();
                let joined = io
                    .export(conn, invite::EXPORTER_LABEL)
                    .and_then(|exported| self.host.join(&name, proof, &exported, &peer.spki));
                match joined {
                    Ok(()) => {
                        peer.client = Some(name);
                        stream.end(io, conn, id, Tag::Joined, &[]);
                    }
                    Err(_) => stream.end(io, conn, id, Tag::Refused, NOT_INVITED.as_bytes()),
                }
            }
            Tag::Stdin if stream.asked => {
                if stream.stdin.is_some() {
                    stream.held = Some(payload);
                }
            }
            Tag::StdinEnd if stream.asked => stream.stdin_ends = true,
            other => {
                let why = format!("a frame out of place ({other:?})");
                stream.end(io, conn, id, Tag::Refused, why.as_bytes());
            }
        }
        Taken::Frame
    }

    /// Acts on a frame of control stream `id` that steers the command on stream `target`:
    /// the first, which makes it the command's, or a resize or a signal. A control stream for
    /// none of this connection's commands is refused.
    fn steer(
        &mut self,
        io: &mut Io<'_>,
        conn: ConnKey,
        id: StreamId,
        target: StreamId,
        tag: Tag,
        payload: &[u8],
    ) {
        let Some(peer) = self.peers.get_mut(&conn) else {
            return;
        };
        let command = peer
            .streams
            .get(&target)
            .filter(|t| t.steers.is_none() && t.running());
        match tag {
            Tag::Controls => match (command.is_some(), peer.streams.get_mut(&id)) {
                (true, Some(stream)) => {
                    stream.steers = Some(target);
                    // Nothing is answered on it.
                    let _ = io.finish(conn, id);
                }
                (false, Some(stream)) => {
                    stream.end(
                        io,
                        conn,
                        id,
                        Tag::Refused,
                        b"no command of this connection's to steer",
                    );
                }
                (_, None) => {}
            },
            Tag::Resize => {
                if let (Some(resize), Some((rows, cols))) =
                    (command.and_then(|c| c.resize.as_ref()), frame::read_size(payload))
                {
                    resize(rows, cols);
                }
            }
            Tag::Signal => {
                if let (Some(signal), Ok(linux)) = (
                    command.and_then(|c| c.signal.as_ref()),
                    <[u8; 4]>::try_from(payload),
                ) {
                    signal(u32::from_be_bytes(linux));
                }
            }
            _ => {}
        }
    }

    /// Lets go of `id` once nothing more goes on it either way: its last frame written whole,
    /// or its client gone from it, whose command, still running, is hung up; a control
    /// stream's once its client ends it. A command's control streams go with it.
    fn settle(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        let Some(peer) = self.peers.get_mut(&conn) else {
            return;
        };
        let Some(stream) = peer.streams.get(&id) else {
            return;
        };
        let steering = stream.steers.is_some();
        if stream.gone {
            if stream.running()
                && let Some(hangup) = &stream.hangup
            {
                hangup();
            }
            io.reset(conn, id);
            io.stop(conn, id);
        } else if !stream.finished && !(steering && stream.ended) {
            return;
        }
        peer.streams.remove(&id);
        if !steering {
            let controls: Vec<StreamId> = peer
                .streams
                .iter()
                .filter(|(_, s)| s.steers == Some(id))
                .map(|(control, _)| *control)
                .collect();
            for control in controls {
                io.stop(conn, control);
                io.reset(conn, control);
                peer.streams.remove(&control);
            }
        }
    }
}

/// What [`relay`] keeps of a command for its stream: where its stdin goes, its output's
/// credits, and what hangs it up, sizes its terminal and signals it.
struct Relayed {
    stdin: mpsc::SyncSender<Vec<u8>>,
    credits: Vec<mpsc::SyncSender<()>>,
    /// The command tells what it made ([`Running::made`]).
    tells: bool,
    hangup: Box<dyn Fn() + Send>,
    resize: Option<Box<dyn Fn(u16, u16) + Send>>,
    signal: Box<dyn Fn(u32) + Send>,
}

/// Relays `running`'s stdio: a thread that writes to its stdin what the client sends, one
/// for each of its stdout and stderr that sends the driver what it writes as credit allows,
/// and one that waits for it to end.
fn relay(
    running: Running,
    conn: ConnKey,
    stream: StreamId,
    sender: &Sender<Event>,
) -> Result<Relayed, Error> {
    let Running {
        mut stdin,
        stdout,
        stderr,
        wait,
        hangup,
        resize,
        signal,
        made,
    } = running;
    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .map(drop)
            .map_err(|e| Error::new(format!("a thread for a client's command: {e}")))
    };
    let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let to = sender.try_clone()?;
    spawn(
        "remote-stdin",
        Box::new(move || {
            // The sender dropped: the command reads the end of its input, this one's drop.
            while let Ok(bytes) = stdin_rx.recv() {
                if stdin.write_all(&bytes).and_then(|()| stdin.flush()).is_err() {
                    // The command reads no more of it: what the client sends goes nowhere,
                    // and the driver, which may wait for room, reads the stream on.
                    drop(stdin_rx);
                    let _ = to.send(Command::Handler(Event::InputTaken { conn, stream }));
                    return;
                }
                let _ = to.send(Command::Handler(Event::InputTaken { conn, stream }));
            }
        }),
    )?;
    let mut credits = Vec::new();
    for (tag, mut from) in [(Tag::Stdout, stdout), (Tag::Stderr, stderr)] {
        let (credit_tx, credit_rx) = mpsc::sync_channel::<()>(1);
        let _ = credit_tx.try_send(());
        credits.push(credit_tx);
        let to = sender.try_clone()?;
        spawn(
            "remote-out",
            Box::new(move || {
                let mut buffer = vec![0u8; 64 << 10];
                while credit_rx.recv().is_ok() {
                    match from.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let bytes = buffer.get(..n).unwrap_or_default().to_vec();
                            if to
                                .send(Command::Handler(Event::Output {
                                    conn,
                                    stream,
                                    tag,
                                    bytes,
                                }))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                let _ = to.send(Command::Handler(Event::OutputEnd { conn, stream, tag }));
            }),
        )?;
    }
    let tells = made.is_some();
    if let Some(made) = made {
        let to = sender.try_clone()?;
        spawn(
            "remote-made",
            Box::new(move || {
                let mut bytes = Vec::new();
                let _ = made.take(MADE_MOST).read_to_end(&mut bytes);
                if !bytes.is_empty() {
                    let _ = to.send(Command::Handler(Event::Output {
                        conn,
                        stream,
                        tag: Tag::Made,
                        bytes,
                    }));
                }
                let _ = to.send(Command::Handler(Event::OutputEnd {
                    conn,
                    stream,
                    tag: Tag::Made,
                }));
            }),
        )?;
    }
    let to = sender.try_clone()?;
    spawn(
        "remote-wait",
        Box::new(move || {
            let status = wait();
            let _ = to.send(Command::Handler(Event::Exited { conn, stream, status }));
        }),
    )?;
    Ok(Relayed {
        stdin: stdin_tx,
        credits,
        tells,
        hangup,
        resize,
        signal,
    })
}

impl<H: Host> Handler for Server<H> {
    type Command = Event;

    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        let client = self.host.enrolled(peer);
        // A peer the machine has not enrolled connects to join alone, while an invitation is
        // free for it.
        if client.is_none() && self.strangers().count() >= self.host.invited() {
            return Err(Close {
                code: UNENROLLED,
                why: "not enrolled",
            });
        }
        self.peers.insert(
            conn,
            Peer {
                spki: peer.to_vec(),
                client,
                streams: HashMap::new(),
                since: io.now(),
            },
        );
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        if let Some(peer) = self.peers.get_mut(&conn) {
            peer.streams.insert(id, Stream::default());
            self.serve(io, conn, id);
        }
    }

    /// A stream let go is read no more: what its client still sends goes unread.
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        self.serve(io, conn, id);
    }

    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        if let Some(stream) = self.stream(conn, id) {
            stream.flush(io, conn, id);
        }
        self.settle(io, conn, id);
    }

    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        if let Some(stream) = self.stream(conn, id) {
            stream.gone = true;
        }
        self.settle(io, conn, id);
    }

    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        // Its commands lose their terminal.
        if let Some(peer) = self.peers.remove(&conn) {
            for stream in peer.streams.values() {
                if stream.running()
                    && let Some(hangup) = &stream.hangup
                {
                    hangup();
                }
            }
        }
    }

    fn command(&mut self, io: &mut Io<'_>, event: Event) {
        let (Event::Output { conn, stream: id, .. }
        | Event::OutputEnd { conn, stream: id, .. }
        | Event::Exited { conn, stream: id, .. }
        | Event::InputTaken { conn, stream: id }) = &event;
        let (conn, id) = (*conn, *id);
        if let Event::InputTaken { .. } = event {
            return self.serve(io, conn, id);
        }
        let Some(stream) = self.stream(conn, id) else {
            return;
        };
        match event {
            Event::Output { tag, bytes, .. } => {
                if frame::put(&mut stream.pending, tag, &bytes).is_ok() {
                    stream.starved = true;
                }
            }
            Event::OutputEnd { tag: Tag::Stdout, .. } => stream.stdout_done = true,
            Event::OutputEnd { tag: Tag::Made, .. } => stream.made_done = true,
            Event::OutputEnd { .. } => stream.stderr_done = true,
            Event::Exited { status, .. } => stream.status = Some(status),
            Event::InputTaken { .. } => {}
        }
        stream.exit_when_done(io, conn, id);
        self.settle(io, conn, id);
    }

    fn done(&self) -> bool {
        false
    }

    fn due(&self) -> Option<Instant> {
        self.strangers().filter_map(|(_, due)| due).min()
    }

    /// A peer the machine has not enrolled that has not joined in its time is let go.
    fn timeout(&mut self, io: &mut Io<'_>, now: Instant) {
        let late: Vec<ConnKey> = self
            .strangers()
            .filter(|(_, due)| due.is_some_and(|at| at <= now))
            .map(|(conn, _)| conn)
            .collect();
        for conn in late {
            io.close(conn, UNENROLLED, "not enrolled in time");
            self.peers.remove(&conn);
        }
    }
}

/// Stops a [`serve`]: each connection closed, its peer told.
#[derive(Debug)]
pub struct Stopper(Sender<Event>);

impl Stopper {
    pub fn stop(&self) -> Result<(), Error> {
        self.0.send(Command::Stop)
    }
}

/// A daemon's listener on `at`, serving `host`'s clients until its [`Stopper`] stops it;
/// `ready` is given the address it is bound to, and the stopper, before it serves.
pub fn serve<H: Host>(
    identity: &Identity,
    at: SocketAddr,
    host: H,
    ready: impl FnOnce(SocketAddr, Stopper),
) -> Result<(), Error> {
    let config = tls::server(identity)?;
    let server = Server {
        host,
        peers: HashMap::new(),
        sender: None,
    };
    let (mut driver, sender) = Driver::bind(at, Some(config), server)?;
    driver.handler_mut().sender = Some(sender.try_clone()?);
    ready(driver.local()?, Stopper(sender));
    driver.run()
}
