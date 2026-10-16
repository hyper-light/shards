//! The daemon's half (D180): the clients it enrolled run commands, each on a stream of its own,
//! and a client that is not enrolled may only join by an invitation. What a client asks of the
//! machine is its [`Host`]'s to judge and run.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::mpsc;

use hyper_quic::StreamId;

use crate::frame::{self, Request, Tag};
use crate::identity::Identity;
use crate::invite;
use crate::net::{Command, ConnKey, Driver, Handler, Io, Read as StreamRead, Sender};
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
}

/// The machine a daemon serves its clients on.
pub trait Host: Send {
    /// The name of the enrolled client whose key's SubjectPublicKeyInfo is `spki`, if one is.
    fn enrolled(&self, spki: &[u8]) -> Option<String>;
    /// Redeems invitation `name`, whose secret `proof` must prove over the connection whose
    /// exporter gave `exported`, enrolling the client `spki` under it.
    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error>;
    /// Starts `request`'s command for the enrolled client `client`.
    fn start(&self, client: &str, request: Request) -> Result<Running, Error>;
}

/// The daemon's answer to any join that failed, whatever made it fail: a refusal tells
/// nothing of the invitations it holds.
pub const NOT_INVITED: &str = "this invitation is not one this machine holds";

/// The daemon's answer to a command from a client it has not enrolled.
pub const NOT_ENROLLED: &str = "this machine has not enrolled you: join it by an invitation first";

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
}

#[derive(Default)]
struct Stream {
    inbound: Vec<u8>,
    pending: Vec<u8>,
    stdin: Option<mpsc::SyncSender<Option<Vec<u8>>>>,
    credits: Vec<mpsc::SyncSender<()>>,
    starved: bool,
    stdout_done: bool,
    stderr_done: bool,
    status: Option<i32>,
    hangup: Option<Box<dyn Fn() + Send>>,
    /// The request frame came.
    asked: bool,
    /// The stream's last frame is queued: its exit, refusal or join.
    last: bool,
    finished: bool,
}

impl Stream {
    fn flush(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        while !self.pending.is_empty() {
            match io.write(conn, id, &self.pending) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    self.pending.drain(..n.min(self.pending.len()));
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

    /// Queues `tag`'s frame as the stream's last.
    fn end(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId, tag: Tag, payload: &[u8]) {
        if self.last {
            return;
        }
        if frame::put(&mut self.pending, tag, payload).is_err() {
            let _ = frame::put(&mut self.pending, Tag::Refused, b"an answer too long");
        }
        self.last = true;
        self.flush(io, conn, id);
    }

    /// Queues the exit status once the command has ended and all it wrote is queued.
    fn exit_when_done(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        if let Some(status) = self.status
            && self.stdout_done
            && self.stderr_done
        {
            self.end(io, conn, id, Tag::Exit, &status.to_be_bytes());
        } else {
            self.flush(io, conn, id);
        }
    }
}

struct Peer {
    spki: Vec<u8>,
    client: Option<String>,
    streams: HashMap<StreamId, Stream>,
}

struct Server<H: Host> {
    host: H,
    peers: HashMap<ConnKey, Peer>,
    sender: Option<Sender<Event>>,
}

impl<H: Host> Server<H> {
    fn frames(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        loop {
            let Some(peer) = self.peers.get_mut(&conn) else {
                return;
            };
            let Some(stream) = peer.streams.get_mut(&id) else {
                return;
            };
            let (tag, payload) = match frame::take(&mut stream.inbound) {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(e) => return stream.end(io, conn, id, Tag::Refused, e.to_string().as_bytes()),
            };
            match tag {
                Tag::Exec if !stream.asked => {
                    stream.asked = true;
                    let Some(client) = peer.client.clone() else {
                        return stream.end(io, conn, id, Tag::Refused, NOT_ENROLLED.as_bytes());
                    };
                    let started =
                        Request::decode(&payload).and_then(|request| self.host.start(&client, request));
                    let relayed = match (started, &self.sender) {
                        (Ok(running), Some(sender)) => relay(running, conn, id, sender),
                        (Ok(_), None) => Err(Error::new("the daemon's listener is not running")),
                        (Err(e), _) => Err(e),
                    };
                    let Some(stream) = self.peers.get_mut(&conn).and_then(|p| p.streams.get_mut(&id)) else {
                        return;
                    };
                    match relayed {
                        Ok((stdin, credits, hangup)) => {
                            stream.stdin = Some(stdin);
                            stream.credits = credits;
                            stream.hangup = Some(hangup);
                        }
                        Err(e) => return stream.end(io, conn, id, Tag::Refused, e.to_string().as_bytes()),
                    }
                }
                Tag::Join if !stream.asked => {
                    stream.asked = true;
                    let mut parts = payload.splitn(2, |b| *b == 0);
                    let name = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                    let proof = parts.next().unwrap_or_default().to_vec();
                    let spki = peer.spki.clone();
                    let joined = io
                        .export(conn, invite::EXPORTER_LABEL)
                        .and_then(|exported| self.host.join(&name, &proof, &exported, &spki));
                    let Some(peer) = self.peers.get_mut(&conn) else {
                        return;
                    };
                    let Some(stream) = peer.streams.get_mut(&id) else {
                        return;
                    };
                    match joined {
                        Ok(()) => {
                            peer.client = Some(name);
                            return stream.end(io, conn, id, Tag::Joined, &[]);
                        }
                        Err(_) => return stream.end(io, conn, id, Tag::Refused, NOT_INVITED.as_bytes()),
                    }
                }
                Tag::Stdin if stream.asked => {
                    if let Some(stdin) = &stream.stdin {
                        // Bounded: a command that stops reading its stdin holds its client
                        // back, by the stream's flow control.
                        let _ = stdin.send(Some(payload));
                    }
                }
                Tag::StdinEnd if stream.asked => {
                    if let Some(stdin) = stream.stdin.take() {
                        let _ = stdin.send(None);
                    }
                }
                other => {
                    let why = format!("a frame out of place ({other:?})");
                    return stream.end(io, conn, id, Tag::Refused, why.as_bytes());
                }
            }
        }
    }
}

type Relayed = (
    mpsc::SyncSender<Option<Vec<u8>>>,
    Vec<mpsc::SyncSender<()>>,
    Box<dyn Fn() + Send>,
);

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
    } = running;
    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .map(drop)
            .map_err(|e| Error::new(format!("a thread for a client's command: {e}")))
    };
    let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Option<Vec<u8>>>(4);
    spawn(
        "remote-stdin",
        Box::new(move || {
            while let Ok(Some(bytes)) = stdin_rx.recv() {
                if stdin.write_all(&bytes).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
            // Dropped: the command reads the end of its input.
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
    let to = sender.try_clone()?;
    spawn(
        "remote-wait",
        Box::new(move || {
            let status = wait();
            let _ = to.send(Command::Handler(Event::Exited { conn, stream, status }));
        }),
    )?;
    Ok((stdin_tx, credits, hangup))
}

impl<H: Host> Handler for Server<H> {
    type Command = Event;

    fn connected(&mut self, _io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> bool {
        let client = self.host.enrolled(peer);
        self.peers.insert(
            conn,
            Peer {
                spki: peer.to_vec(),
                client,
                streams: HashMap::new(),
            },
        );
        true
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        loop {
            let read = io.read(conn, id);
            let Some(peer) = self.peers.get_mut(&conn) else {
                return;
            };
            let stream = peer.streams.entry(id).or_default();
            match read {
                StreamRead::Bytes(bytes) => stream.inbound.extend_from_slice(&bytes),
                StreamRead::Blocked => break,
                StreamRead::End => {
                    // The client sent all it will: a command that has not had its stdin's
                    // end reads it now.
                    if let Some(stdin) = stream.stdin.take() {
                        let _ = stdin.send(None);
                    }
                    break;
                }
                StreamRead::Reset => {
                    if let Some(hangup) = &stream.hangup {
                        hangup();
                    }
                    break;
                }
            }
        }
        self.frames(io, conn, id);
    }

    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, id: StreamId) {
        if let Some(stream) = self.peers.get_mut(&conn).and_then(|p| p.streams.get_mut(&id)) {
            stream.flush(io, conn, id);
        }
    }

    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str) {
        // Its commands lose their terminal.
        if let Some(peer) = self.peers.remove(&conn) {
            for stream in peer.streams.values() {
                if stream.status.is_none()
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
        | Event::Exited { conn, stream: id, .. }) = &event;
        let (conn, id) = (*conn, *id);
        let Some(stream) = self.peers.get_mut(&conn).and_then(|p| p.streams.get_mut(&id)) else {
            return;
        };
        match event {
            Event::Output { tag, bytes, .. } => {
                if frame::put(&mut stream.pending, tag, &bytes).is_ok() {
                    stream.starved = true;
                }
            }
            Event::OutputEnd { tag: Tag::Stdout, .. } => stream.stdout_done = true,
            Event::OutputEnd { .. } => stream.stderr_done = true,
            Event::Exited { status, .. } => stream.status = Some(status),
        }
        stream.exit_when_done(io, conn, id);
    }

    fn done(&self) -> bool {
        false
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
