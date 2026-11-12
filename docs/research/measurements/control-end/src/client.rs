//! The controller's side of one microVM's control: one QUIC connection on a driver and thread
//! of its own, as each of a controller's connections is its own. Its times are taken on the
//! driver's thread as the driver turns: from handing bytes to QUIC to having them all back.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Instant;

use hyper_quic::{ClientConfig, StreamId};
use shards_remote::net::{Close, ConnKey, Handler, Io, Read};

use crate::proto::{self, CHUNK, Header};

/// What the run asks of a connection.
#[derive(Debug, Clone, Copy)]
pub enum Ask {
    Connect,
    /// `rounds` round trips of `size` bytes each, one at a time.
    Ping { rounds: u32, size: usize },
    Upload(u64),
    Download(u64),
    Stats,
    Close,
}

/// What a connection tells the run.
#[derive(Debug)]
pub enum Said {
    /// Its handshake took `handshake_ns` and `cpu_ns` of this thread's CPU; its first round
    /// trip came back `first_ns` after it began to connect.
    Connected { vm: u32, handshake_ns: u64, cpu_ns: u64, first_ns: u64 },
    Pinged { vm: u32, rtt_ns: Vec<u64> },
    Moved { vm: u32, upload: bool, bytes: u64, ns: u64 },
    Stats { vm: u32, line: String },
    Failed { vm: u32, what: &'static str, why: String },
}

#[derive(Debug)]
enum Phase {
    Idle,
    Connecting { began: Instant, cpu: u64 },
    First { began: Instant, handshake_ns: u64, cpu_ns: u64, got: usize },
    Ping { left: u32, size: usize, t0: Instant, got: usize, rtts: Vec<u64> },
    Upload { stream: StreamId, left: u64, total: u64, t0: Instant, ack: Vec<u8> },
    Download { stream: StreamId, total: u64, got: u64, t0: Instant },
    Stats { stream: StreamId, text: Vec<u8> },
}

#[derive(Debug)]
pub struct Client {
    vm: u32,
    to: SocketAddr,
    config: Option<ClientConfig>,
    said: mpsc::Sender<Said>,
    first_size: usize,
    conn: Option<ConnKey>,
    /// The round trips' stream, opened as the connection's handshake is done.
    echo: Option<StreamId>,
    /// Bytes for `echo` or an operation's stream not yet taken by QUIC, and whether the
    /// stream is finished once they are.
    out: Option<(StreamId, Vec<u8>, usize, bool)>,
    phase: Phase,
    closed: bool,
}

impl Client {
    pub fn new(vm: u32, to: SocketAddr, config: ClientConfig, said: mpsc::Sender<Said>, first_size: usize) -> Client {
        Client {
            vm,
            to,
            config: Some(config),
            said,
            first_size,
            conn: None,
            echo: None,
            out: None,
            phase: Phase::Idle,
            closed: false,
        }
    }

    fn fail(&mut self, what: &'static str, why: String) {
        self.phase = Phase::Idle;
        let _ = self.said.send(Said::Failed { vm: self.vm, what, why });
    }

    /// `bytes` on `stream`, as much as QUIC takes now, the rest when it may take more; the
    /// stream finished after them where `finish` says.
    fn send(&mut self, io: &mut Io<'_>, stream: StreamId, bytes: Vec<u8>, finish: bool) {
        self.out = Some((stream, bytes, 0, finish));
        self.flush(io);
    }

    fn flush(&mut self, io: &mut Io<'_>) {
        let Some(conn) = self.conn else {
            return;
        };
        if let Some((stream, bytes, sent, finish)) = &mut self.out {
            while *sent < bytes.len() {
                match io.write(conn, *stream, bytes.get(*sent..).unwrap_or_default()) {
                    Ok(0) => return,
                    Ok(n) => *sent = sent.saturating_add(n),
                    Err(e) => {
                        self.out = None;
                        self.fail("write", e.to_string());
                        return;
                    }
                }
            }
            if *finish {
                let _ = io.finish(conn, *stream);
            }
            self.out = None;
        }
        // An upload's bytes, once its header is out.
        if let Phase::Upload { stream, left, .. } = &mut self.phase {
            while *left > 0 {
                let n = usize::try_from(*left).unwrap_or(CHUNK).min(CHUNK);
                match io.write(conn, *stream, proto::ZEROS.get(..n).unwrap_or_default()) {
                    Ok(0) => return,
                    Ok(n) => *left = left.saturating_sub(n as u64),
                    Err(e) => {
                        let why = e.to_string();
                        self.fail("upload", why);
                        return;
                    }
                }
            }
            let stream = *stream;
            let _ = io.finish(conn, stream);
        }
    }

    fn open(&mut self, io: &mut Io<'_>, header: Header) -> Option<StreamId> {
        let conn = self.conn?;
        match io.open(conn) {
            Ok(stream) => Some(stream),
            Err(e) => {
                self.fail("open", format!("{e} ({:?})", header.op as char));
                None
            }
        }
    }

    fn ping(&mut self, io: &mut Io<'_>, size: usize) {
        if let Some(echo) = self.echo {
            self.send(io, echo, vec![0u8; size], false);
        }
    }

    fn header(&self, op: u8, count: u64) -> Header {
        Header { vm: self.vm, op, count }
    }

    fn read_echo(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let n = match io.read(conn, stream) {
            Read::Bytes(b) => b.len(),
            Read::Blocked => return,
            Read::End(_) | Read::Reset => {
                self.fail("echo", "the echo stream ended".into());
                return;
            }
        };
        let now = Instant::now();
        let vm = self.vm;
        let first_size = self.first_size;
        match &mut self.phase {
            Phase::First {
                began,
                handshake_ns,
                cpu_ns,
                got,
            } => {
                *got = got.saturating_add(n);
                if *got >= first_size {
                    let said = Said::Connected {
                        vm,
                        handshake_ns: *handshake_ns,
                        cpu_ns: *cpu_ns,
                        first_ns: nanos(now.saturating_duration_since(*began)),
                    };
                    self.phase = Phase::Idle;
                    let _ = self.said.send(said);
                }
            }
            Phase::Ping {
                left,
                size,
                t0,
                got,
                rtts,
            } => {
                *got = got.saturating_add(n);
                if *got < *size {
                    return;
                }
                rtts.push(nanos(now.saturating_duration_since(*t0)));
                *left = left.saturating_sub(1);
                if *left == 0 {
                    let rtt_ns = std::mem::take(rtts);
                    self.phase = Phase::Idle;
                    let _ = self.said.send(Said::Pinged { vm, rtt_ns });
                } else {
                    *got = 0;
                    *t0 = Instant::now();
                    let size = *size;
                    self.ping(io, size);
                }
            }
            _ => {}
        }
    }

    fn read_op(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let vm = self.vm;
        let read = io.read(conn, stream);
        let now = Instant::now();
        match (&mut self.phase, read) {
            (Phase::Upload { stream: s, total, t0, ack, .. }, read) if *s == stream => match read {
                Read::Bytes(b) | Read::End(b) => {
                    ack.extend_from_slice(&b);
                    if ack.len() >= 8 {
                        let said = Said::Moved {
                            vm,
                            upload: true,
                            bytes: *total,
                            ns: nanos(now.saturating_duration_since(*t0)),
                        };
                        self.phase = Phase::Idle;
                        let _ = self.said.send(said);
                    }
                }
                Read::Blocked => {}
                Read::Reset => self.fail("upload", "reset".into()),
            },
            (Phase::Download { stream: s, total, got, t0 }, read) if *s == stream => match read {
                Read::Bytes(b) => *got = got.saturating_add(b.len() as u64),
                Read::End(b) => {
                    *got = got.saturating_add(b.len() as u64);
                    let said = if *got == *total {
                        Said::Moved {
                            vm,
                            upload: false,
                            bytes: *total,
                            ns: nanos(now.saturating_duration_since(*t0)),
                        }
                    } else {
                        Said::Failed {
                            vm,
                            what: "download",
                            why: format!("{got} of {total} bytes"),
                        }
                    };
                    self.phase = Phase::Idle;
                    let _ = self.said.send(said);
                }
                Read::Blocked => {}
                Read::Reset => self.fail("download", "reset".into()),
            },
            (Phase::Stats { stream: s, text }, read) if *s == stream => match read {
                Read::Bytes(b) => text.extend_from_slice(&b),
                Read::End(b) => {
                    text.extend_from_slice(&b);
                    let line = String::from_utf8_lossy(text).trim().to_string();
                    self.phase = Phase::Idle;
                    let _ = self.said.send(Said::Stats { vm, line });
                }
                Read::Blocked => {}
                Read::Reset => self.fail("stats", "reset".into()),
            },
            _ => {}
        }
    }
}

fn nanos(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Handler for Client {
    type Command = Ask;

    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _peer: &[u8]) -> Result<(), Close> {
        // Its key was pinned at the handshake (tls::client).
        let now = Instant::now();
        let Phase::Connecting { began, cpu } = self.phase else {
            return Ok(());
        };
        let handshake_ns = nanos(now.saturating_duration_since(began));
        let cpu_ns = crate::procs::thread_cpu_ns().saturating_sub(cpu);
        self.conn = Some(conn);
        let header = self.header(proto::ECHO, 0);
        let Some(echo) = self.open(io, header) else {
            return Ok(());
        };
        self.echo = Some(echo);
        self.phase = Phase::First {
            began,
            handshake_ns,
            cpu_ns,
            got: 0,
        };
        let mut first = header.encode().to_vec();
        first.resize(proto::HEADER.saturating_add(self.first_size), 0);
        self.send(io, echo, first, false);
        Ok(())
    }

    fn opened(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if Some(stream) == self.echo {
            self.read_echo(io, conn, stream);
        } else {
            self.read_op(io, conn, stream);
        }
    }

    fn writable(&mut self, io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.flush(io);
    }

    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.fail("stream", "the endpoint stopped reading".into());
    }

    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, why: &str, code: Option<u64>) {
        self.closed = true;
        self.fail("connection", format!("lost: {why} (code {code:?})"));
    }

    fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
        match ask {
            Ask::Connect => {
                let Some(config) = self.config.take() else {
                    return self.fail("connect", "connected already".into());
                };
                let cpu = crate::procs::thread_cpu_ns();
                let began = Instant::now();
                match io.connect(self.to, config) {
                    Ok(_) => self.phase = Phase::Connecting { began, cpu },
                    Err(e) => self.fail("connect", e.to_string()),
                }
            }
            Ask::Ping { rounds, size } => {
                if rounds == 0 {
                    let _ = self.said.send(Said::Pinged {
                        vm: self.vm,
                        rtt_ns: Vec::new(),
                    });
                    return;
                }
                self.phase = Phase::Ping {
                    left: rounds,
                    size,
                    t0: Instant::now(),
                    got: 0,
                    rtts: Vec::with_capacity(rounds as usize),
                };
                self.ping(io, size);
            }
            Ask::Upload(total) => {
                let header = self.header(proto::UPLOAD, total);
                let Some(stream) = self.open(io, header) else {
                    return;
                };
                self.phase = Phase::Upload {
                    stream,
                    left: total,
                    total,
                    t0: Instant::now(),
                    ack: Vec::new(),
                };
                self.send(io, stream, header.encode().to_vec(), false);
            }
            Ask::Download(total) => {
                let header = self.header(proto::DOWNLOAD, total);
                let Some(stream) = self.open(io, header) else {
                    return;
                };
                self.phase = Phase::Download {
                    stream,
                    total,
                    got: 0,
                    t0: Instant::now(),
                };
                self.send(io, stream, header.encode().to_vec(), true);
            }
            Ask::Stats => {
                let header = self.header(proto::STATS, 0);
                let Some(stream) = self.open(io, header) else {
                    return;
                };
                self.phase = Phase::Stats {
                    stream,
                    text: Vec::new(),
                };
                self.send(io, stream, header.encode().to_vec(), true);
            }
            Ask::Close => {
                if let Some(conn) = self.conn {
                    io.close(conn, 0, "the measurement is done");
                }
                self.closed = true;
            }
        }
    }

    fn done(&self) -> bool {
        self.closed
    }
}
