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
use crate::net::{Close, Command, ConnKey, Driver, Handler, Io, Read as StreamRead, Sender};
use crate::{Error, server, tls};

/// How many bytes of a stream's frames wait unsent before its reader waits too: a QUIC
/// stream's own receive window upstream, which the connection's flow control then keeps.
const HIGH_WATER: usize = 1 << 20;

/// Where the daemon is: its address, and its key's SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remote {
    pub address: SocketAddr,
    pub daemon: Fingerprint,
}

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
}

/// What the daemon answered.
enum Answer {
    Exit(i32),
    Refused(String),
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
    answer: Option<Answer>,
    failed: Option<String>,
    done: bool,
}

impl Client<'_> {
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
        if self.starved && self.pending.len() < HIGH_WATER {
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
        if let Some(conn) = self.conn {
            io.close(conn, 0, "");
        }
        self.done = true;
    }

    fn answered(&mut self, io: &mut Io<'_>, answer: Answer) {
        self.answer = Some(answer);
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
                    Tag::Joined => {
                        self.answered(io, Answer::Joined);
                        return;
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

    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, why: &str, code: Option<u64>) {
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
            self.failed = Some(match self.stream {
                // Its handshake never ended: no daemon answered there.
                None => format!(
                    "no shards daemon answered at {} within {:?} ({why}): is it listening there (shards run daemon --listen ADDR:PORT)?",
                    self.remote.address,
                    tls::IDLE
                ),
                Some(_) => format!("the connection to {} ended: {why}", self.remote.address),
            });
        }
        self.done = true;
    }

    fn command(&mut self, io: &mut Io<'_>, command: Event) {
        match command {
            Event::Connect => {
                let Some(config) = self.identity.take() else {
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
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
    std::thread::Builder::new()
        .name("remote-out".into())
        .spawn_scoped(scope, move || {
            while let Ok(bytes) = rx.recv() {
                if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
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
    let input = stdin.is_some();
    let answer = run(
        identity,
        remote,
        Ask::Exec { request, input },
        stdin,
        Some((Box::new(stdout), Box::new(stderr))),
        Some(Box::new(steered)),
    )?;
    match answer {
        Answer::Exit(status) => Ok(Ok(status)),
        Answer::Refused(why) => Ok(Err(why)),
        Answer::Joined => Err(Error::new("the daemon answered a command as a join")),
    }
}

/// Joins the daemon `invitation` names, as the home whose identity is `identity`: Ok once
/// the daemon has enrolled it, or the daemon's reason for refusing.
pub fn join(identity: &Identity, invitation: &Invitation) -> Result<Result<(), String>, Error> {
    let remote = Remote {
        address: invitation.address,
        daemon: invitation.daemon,
    };
    let ask = Ask::Join {
        name: invitation.name.clone(),
        secret: invitation.secret,
    };
    match run(identity, remote, ask, None, None, None)? {
        Answer::Joined => Ok(Ok(())),
        Answer::Refused(why) => Ok(Err(why)),
        Answer::Exit(_) => Err(Error::new("the daemon answered a join as a command")),
    }
}

type Outputs = (Box<dyn Write + Send>, Box<dyn Write + Send>);

fn run(
    identity: &Identity,
    remote: Remote,
    ask: Ask,
    stdin: Option<Box<dyn Read + Send>>,
    outputs: Option<Outputs>,
    steered: Option<Box<dyn FnOnce(Controls) + '_>>,
) -> Result<Answer, Error> {
    let config = tls::client(identity, remote.daemon)?;
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
            answer: None,
            failed: None,
            done: false,
        };
        let (mut driver, sender) = Driver::bind(any, None, client)?;
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
                    let mut buffer = vec![0u8; 64 << 10];
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
        sender.send(Command::Handler(Event::Connect))?;
        driver.run()?;
        let mut client = driver.into_handler();
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
