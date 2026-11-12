//! Arm g's endpoint, in the guest: each stream's operation served on D180's QUIC driver, for
//! the one controller whose key it was told.

use std::collections::HashMap;

use hyper_quic::StreamId;
use shards_remote::net::{Close, ConnKey, Handler, Io, Read};

use crate::proto::{self, CHUNK, Header};

/// What a stream is doing.
#[derive(Debug)]
enum Op {
    /// Its header so far.
    Head(Vec<u8>),
    /// Bytes read and not yet all written back; whether the controller has ended its side.
    Echo { out: Vec<u8>, sent: usize, ended: bool },
    /// Bytes still to read, and how many in all.
    Sink { left: u64, total: u64 },
    /// Bytes still to send.
    Source { left: u64 },
    /// An answer being written, then the stream finished.
    Reply { out: Vec<u8>, sent: usize },
    /// Answered and finished: what still comes is read and let go, until its end.
    Drain,
}

#[derive(Debug)]
pub struct Serve {
    controller: [u8; 32],
    streams: HashMap<(ConnKey, StreamId), Op>,
    /// This process's CPU time as it began to serve, and as its first connection's handshake
    /// was done: the handshake's CPU here, as nothing else ran between.
    ready_cpu_us: u64,
    handshake_cpu_us: Option<u64>,
}

impl Serve {
    pub fn new(controller: [u8; 32], ready_cpu_us: u64) -> Serve {
        Serve {
            controller,
            streams: HashMap::new(),
            ready_cpu_us,
            handshake_cpu_us: None,
        }
    }

    fn read(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let key = (conn, stream);
            let Some(op) = self.streams.get_mut(&key) else {
                // A stream answered and let go: its end is read, so QUIC lets it go too.
                let _ = io.read(conn, stream);
                return;
            };
            match op {
                Op::Head(got) => {
                    let want = proto::HEADER.saturating_sub(got.len());
                    match io.read_within(conn, stream, want) {
                        Ok(Read::Bytes(b)) => got.extend_from_slice(&b),
                        Ok(Read::Blocked) => return,
                        Ok(Read::End(_) | Read::Reset) | Err(_) => {
                            self.streams.remove(&key);
                            return;
                        }
                    }
                    if got.len() < proto::HEADER {
                        continue;
                    }
                    let next = match Header::decode(got) {
                        Ok(h) => self.start(h),
                        Err(_) => {
                            io.reset(conn, stream);
                            io.stop(conn, stream);
                            self.streams.remove(&key);
                            return;
                        }
                    };
                    self.streams.insert(key, next);
                    self.write(io, conn, stream);
                }
                Op::Echo { out, sent, ended } => {
                    if *ended || *sent < out.len() {
                        // What was read goes back before more is read: one turn's bytes at
                        // most are held.
                        return;
                    }
                    match io.read_within(conn, stream, CHUNK) {
                        Ok(Read::Bytes(b)) => {
                            *out = b;
                            *sent = 0;
                        }
                        Ok(Read::End(b)) => {
                            *out = b;
                            *sent = 0;
                            *ended = true;
                        }
                        Ok(Read::Blocked) => return,
                        Ok(Read::Reset) | Err(_) => {
                            self.streams.remove(&key);
                            return;
                        }
                    }
                    self.write(io, conn, stream);
                }
                Op::Sink { left, total } => {
                    let want = usize::try_from(*left).unwrap_or(CHUNK).min(CHUNK);
                    let n = match io.read_within(conn, stream, want.max(1)) {
                        Ok(Read::Bytes(b)) => b.len(),
                        Ok(Read::End(b)) => {
                            if b.len() as u64 != *left {
                                self.streams.remove(&key);
                                return;
                            }
                            b.len()
                        }
                        Ok(Read::Blocked) => return,
                        Ok(Read::Reset) | Err(_) => {
                            self.streams.remove(&key);
                            return;
                        }
                    };
                    *left = left.saturating_sub(n as u64);
                    if *left == 0 {
                        let answer = total.to_be_bytes().to_vec();
                        self.streams.insert(key, Op::Reply { out: answer, sent: 0 });
                        self.write(io, conn, stream);
                    }
                }
                Op::Source { .. } | Op::Reply { .. } | Op::Drain => match io.read(conn, stream) {
                    Read::Bytes(_) => {}
                    Read::Blocked => return,
                    Read::End(_) | Read::Reset => {
                        if matches!(self.streams.get(&key), Some(Op::Drain)) {
                            self.streams.remove(&key);
                        }
                        return;
                    }
                },
            }
        }
    }

    fn start(&self, header: Header) -> Op {
        match header.op {
            proto::ECHO => Op::Echo {
                out: Vec::new(),
                sent: 0,
                ended: false,
            },
            proto::UPLOAD if header.count == 0 => Op::Reply {
                out: 0u64.to_be_bytes().to_vec(),
                sent: 0,
            },
            proto::UPLOAD => Op::Sink {
                left: header.count,
                total: header.count,
            },
            proto::DOWNLOAD => Op::Source { left: header.count },
            _ => {
                let extra = format!(
                    "arm=g ready_cpu_us={} handshake_cpu_us={}",
                    self.ready_cpu_us,
                    self.handshake_cpu_us.unwrap_or(0)
                );
                Op::Reply {
                    out: crate::guest::stats_line(&extra).into_bytes(),
                    sent: 0,
                }
            }
        }
    }

    fn write(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let key = (conn, stream);
        let Some(op) = self.streams.get_mut(&key) else {
            return;
        };
        match op {
            Op::Echo { out, sent, ended } => {
                while *sent < out.len() {
                    match io.write(conn, stream, out.get(*sent..).unwrap_or_default()) {
                        Ok(0) => return,
                        Ok(n) => *sent = sent.saturating_add(n),
                        Err(_) => {
                            self.streams.remove(&key);
                            return;
                        }
                    }
                }
                if *ended {
                    // Both sides done: the controller's end was read with its last bytes.
                    let _ = io.finish(conn, stream);
                    self.streams.remove(&key);
                }
            }
            Op::Source { left } => {
                while *left > 0 {
                    let n = usize::try_from(*left).unwrap_or(CHUNK).min(CHUNK);
                    match io.write(conn, stream, proto::ZEROS.get(..n).unwrap_or_default()) {
                        Ok(0) => return,
                        Ok(n) => *left = left.saturating_sub(n as u64),
                        Err(_) => {
                            self.streams.remove(&key);
                            return;
                        }
                    }
                }
                let _ = io.finish(conn, stream);
                self.streams.insert(key, Op::Drain);
            }
            Op::Reply { out, sent } => {
                while *sent < out.len() {
                    match io.write(conn, stream, out.get(*sent..).unwrap_or_default()) {
                        Ok(0) => return,
                        Ok(n) => *sent = sent.saturating_add(n),
                        Err(_) => {
                            self.streams.remove(&key);
                            return;
                        }
                    }
                }
                let _ = io.finish(conn, stream);
                self.streams.insert(key, Op::Drain);
            }
            Op::Head(_) | Op::Sink { .. } | Op::Drain => {}
        }
    }
}

impl Handler for Serve {
    type Command = ();

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        if shards_remote::identity::fingerprint(peer) != self.controller {
            return Err(Close {
                code: 1,
                why: "not the controller this microVM was told",
            });
        }
        if self.handshake_cpu_us.is_none() {
            self.handshake_cpu_us = Some(crate::guest::process_cpu_us().saturating_sub(self.ready_cpu_us));
        }
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.streams
            .insert((conn, stream), Op::Head(Vec::with_capacity(proto::HEADER)));
        // What came with the stream's first frame: quinn-proto tells of a stream the peer
        // opened (Opened) and not of the bytes that opened it (Readable).
        self.read(io, conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.read(io, conn, stream);
    }

    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        // What waited to be sent goes, then what waited behind it is read.
        self.write(io, conn, stream);
        self.read(io, conn, stream);
    }

    fn stopped(&mut self, _io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.streams.remove(&(conn, stream));
    }

    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        self.streams.retain(|(c, _), _| *c != conn);
    }

    fn command(&mut self, _io: &mut Io<'_>, (): ()) {}

    fn done(&self) -> bool {
        false
    }
}
