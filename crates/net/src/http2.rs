//! HTTP/2 (RFC 9113) for the credential injector (D242, kiln plan 01a1d2b88be57304b972761e2a5c6781
//! step 7): the frames it reads and writes, each hop's settings and flow control, and the request
//! field blocks it rewrites. An injected flow is two connections, the guest's and the host's, and
//! the injector is an end of each: field compression is each connection's own (§4.3.1), and so is
//! flow control, hop by hop (§6.9). So a request's block is decoded with the guest's hop's
//! decoder, its credential filled in where `credential::decide` says, and encoded anew with the
//! host's hop's encoder, its credential fields never indexed (RFC 7541 §7.1.3, f42); nothing of
//! one hop's encoded bytes reaches the other. Every bound here is the protocol's (§4.2, §6.5.2,
//! §6.9) or a value its caller derives from what it holds; none is chosen here (R03).
//!
//! Like inject.rs it holds no socket and reads no clock. Its caller is inject.rs' `H2`, an
//! injected flow whose two hops chose `h2` (RFC 7301, RFC 9113 §3.2).

use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};

use zeroize::Zeroizing;

use crate::dns::credential::{self, CredentialGrant, Fill};
use crate::hpack::{self, Field, Indexing};
use crate::inject::{authority_host, token_in};

/// The client's connection preface (§3.4).
pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// A frame's header (§4.1).
pub const HEAD: usize = 9;

// Frame types (§6) and their flags.
pub const DATA: u8 = 0x0;
pub const HEADERS: u8 = 0x1;
pub const RST_STREAM: u8 = 0x3;
pub const SETTINGS: u8 = 0x4;
pub const PUSH_PROMISE: u8 = 0x5;
pub const PING: u8 = 0x6;
pub const GOAWAY: u8 = 0x7;
pub const WINDOW_UPDATE: u8 = 0x8;
pub const CONTINUATION: u8 = 0x9;
pub const END_STREAM: u8 = 0x1;
pub const ACK: u8 = 0x1;
pub const END_HEADERS: u8 = 0x4;
pub const PADDED: u8 = 0x8;
pub const PRIORITY: u8 = 0x20;

/// The most a flow-control window may be, and so SETTINGS_INITIAL_WINDOW_SIZE (§6.5.2, §6.9.1).
const MOST_WINDOW: i64 = (1 << 31) - 1;
/// The initial size of every window, a connection's too, until a peer says another (§6.9.2).
const INITIAL_WINDOW: u32 = 65_535;
/// What SETTINGS_MAX_FRAME_SIZE may be (§6.5.2), its least the initial value.
const FRAME_SIZES: RangeInclusive<u32> = (1 << 14)..=(1 << 24) - 1;

/// An error code (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    NoError = 0x0,
    Protocol = 0x1,
    Internal = 0x2,
    FlowControl = 0x3,
    StreamClosed = 0x5,
    FrameSize = 0x6,
    RefusedStream = 0x7,
    Cancel = 0x8,
    Compression = 0x9,
    EnhanceYourCalm = 0xb,
}

/// What an error ends (§5.4): the connection, with a GOAWAY, or one stream, with a RST_STREAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Connection,
    Stream(u32),
}

/// An HTTP/2 error, typed (R04): what it ends, its code, and why, for the audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub scope: Scope,
    pub code: Code,
    pub why: &'static str,
}

pub fn connection(code: Code, why: &'static str) -> Error {
    Error {
        scope: Scope::Connection,
        code,
        why,
    }
}

/// A frame's header (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub length: u32,
    pub kind: u8,
    pub flags: u8,
    pub stream: u32,
}

impl Head {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.length.to_be_bytes().get(1..).unwrap_or_default());
        out.extend_from_slice(&[self.kind, self.flags]);
        out.extend_from_slice(&(self.stream & MOST_STREAM).to_be_bytes());
    }
}

/// A stream identifier's 31 bits; the reserved bit is ignored as it is read (§4.1).
pub const MOST_STREAM: u32 = (1 << 31) - 1;

/// The first whole frame of `bytes`: its header and payload, none until it is whole. One longer
/// than `most`, this side's SETTINGS_MAX_FRAME_SIZE, is a connection error FRAME_SIZE_ERROR (§4.2).
pub fn frame(bytes: &[u8], most: u32) -> Result<Option<(Head, &[u8])>, Error> {
    let Some(&[l0, l1, l2, kind, flags, s0, s1, s2, s3]) = bytes.get(..HEAD) else {
        return Ok(None);
    };
    let head = Head {
        length: u32::from_be_bytes([0, l0, l1, l2]),
        kind,
        flags,
        stream: u32::from_be_bytes([s0, s1, s2, s3]) & MOST_STREAM,
    };
    if head.length > most {
        return Err(connection(
            Code::FrameSize,
            "a frame past SETTINGS_MAX_FRAME_SIZE",
        ));
    }
    let end = HEAD.saturating_add(head.length as usize);
    Ok(bytes.get(HEAD..end).map(|payload| (head, payload)))
}

/// What a peer says of itself (§6.5.2), each setting's initial value until it says another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub header_table_size: u32,
    pub initial_window_size: u32,
    pub max_frame_size: u32,
    /// SETTINGS_MAX_CONCURRENT_STREAMS: none until the peer says one, "initially, there is no
    /// limit" (§6.5.2).
    pub max_concurrent_streams: Option<u32>,
}

impl Settings {
    pub const INITIAL: Settings = Settings {
        header_table_size: 4096,
        initial_window_size: INITIAL_WINDOW,
        max_frame_size: 1 << 14,
        max_concurrent_streams: None,
    };

    /// A SETTINGS frame's parameters, each six octets, applied in order (§6.5.1); one unknown,
    /// or one this side keeps no state for, is passed over (§6.5.2).
    fn apply(&mut self, payload: &[u8]) -> Result<(), Error> {
        if !payload.len().is_multiple_of(6) {
            return Err(connection(
                Code::FrameSize,
                "a SETTINGS frame of no whole parameters",
            ));
        }
        for &[i0, i1, v0, v1, v2, v3] in payload.as_chunks::<6>().0 {
            let value = u32::from_be_bytes([v0, v1, v2, v3]);
            match u16::from_be_bytes([i0, i1]) {
                0x1 => self.header_table_size = value,
                0x2 if value > 1 => return Err(connection(Code::Protocol, "SETTINGS_ENABLE_PUSH past 1")),
                0x3 => self.max_concurrent_streams = Some(value),
                0x4 if i64::from(value) > MOST_WINDOW => {
                    return Err(connection(
                        Code::FlowControl,
                        "SETTINGS_INITIAL_WINDOW_SIZE past 2^31-1",
                    ));
                }
                0x4 => self.initial_window_size = value,
                0x5 if !FRAME_SIZES.contains(&value) => {
                    return Err(connection(
                        Code::Protocol,
                        "SETTINGS_MAX_FRAME_SIZE out of its range",
                    ));
                }
                0x5 => self.max_frame_size = value,
                _ => {}
            }
        }
        Ok(())
    }
}

/// One side's flow-control windows on a hop (§6.9): the connection's, and each stream's, which
/// starts at the receiver's SETTINGS_INITIAL_WINDOW_SIZE and moves with it (§6.9.2), perhaps
/// below zero. A stream not yet seen has the initial window.
#[derive(Debug)]
struct Windows {
    connection: i64,
    initial: i64,
    streams: BTreeMap<u32, i64>,
}

impl Windows {
    fn new(initial: u32) -> Windows {
        Windows {
            connection: i64::from(INITIAL_WINDOW),
            initial: i64::from(initial),
            streams: BTreeMap::new(),
        }
    }

    fn stream(&self, stream: u32) -> i64 {
        self.streams.get(&stream).copied().unwrap_or(self.initial)
    }

    /// SETTINGS_INITIAL_WINDOW_SIZE changed: each stream's window moved by the difference; one
    /// past 2^31-1 is a connection error FLOW_CONTROL_ERROR (§6.9.2).
    fn set_initial(&mut self, initial: u32) -> Result<(), Error> {
        let by = i64::from(initial) - self.initial;
        for window in self.streams.values_mut() {
            *window = window.saturating_add(by);
            if *window > MOST_WINDOW {
                return Err(connection(Code::FlowControl, "a stream's window past 2^31-1"));
            }
        }
        self.initial = i64::from(initial);
        Ok(())
    }

    /// A window grown by a WINDOW_UPDATE's increment: the connection's on stream 0. Past 2^31-1
    /// it ends the stream or the connection with FLOW_CONTROL_ERROR (§6.9.1).
    fn grow(&mut self, stream: u32, by: u32) -> Result<(), Error> {
        let (window, scope) = match stream {
            0 => (&mut self.connection, Scope::Connection),
            s => (self.streams.entry(s).or_insert(self.initial), Scope::Stream(s)),
        };
        *window = window.saturating_add(i64::from(by));
        if *window > MOST_WINDOW {
            return Err(Error {
                scope,
                code: Code::FlowControl,
                why: "a window past 2^31-1",
            });
        }
        Ok(())
    }

    fn take(&mut self, stream: u32, n: usize) {
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        self.connection = self.connection.saturating_sub(n);
        let window = self.streams.entry(stream).or_insert(self.initial);
        *window = window.saturating_sub(n);
    }
}

/// What this side holds of a hop's peer's DATA before passing it on: on one stream and on the
/// connection. Granted as its windows, a window being "a measure of the buffering capacity of the
/// receiver" (§6.9.1), so the peer sends no more than this side holds. Its caller derives both
/// from what its connection holds (tcp.rs' `TO_HOST` and `TO_GUEST`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holds {
    pub stream: u32,
    pub connection: u32,
}

/// Which end of a hop this side is (§3.4): the client sends the connection preface's octets
/// first, and with them turns server push off (§6.5.2, SETTINGS_ENABLE_PUSH 0), as the injector
/// passes no pushed response on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Client,
    Server,
}

/// One hop of an injected HTTP/2 flow, the injector one end of it: what its peer says of itself,
/// each side's windows, the credit this side owes back, and the field compression each way, none
/// of it shared with the other hop (§4.3.1, §6.9).
#[derive(Debug)]
pub struct Hop {
    pub peer: Settings,
    holds: Holds,
    send: Windows,
    recv: Windows,
    /// Octets of the peer's passed on but not yet given back: the connection's, then each
    /// stream's.
    owed: (u32, BTreeMap<u32, u32>),
    pub decoder: hpack::Decoder,
    pub encoder: hpack::Encoder,
}

impl Hop {
    /// A hop whose peer holds the protocol's initial settings until it says otherwise, this side
    /// holding `holds` of its DATA and its decoder the protocol's table (§6.5.2), its encoder's
    /// no larger than `most_table`: what this side lets the other hop's peer keep, so that one
    /// hop's table costs no more than the other's.
    pub fn new(holds: Holds, most_table: usize) -> Hop {
        let initial = Settings::INITIAL;
        let table = initial.header_table_size as usize;
        // Until the peer acknowledges this side's settings it may send to the initial windows
        // (§6.9.3), and the connection's is never less than its initial size (§6.9.2).
        let mut recv = Windows::new(holds.stream.max(INITIAL_WINDOW));
        recv.connection = i64::from(holds.connection.max(INITIAL_WINDOW));
        Hop {
            peer: initial,
            holds,
            send: Windows::new(initial.initial_window_size),
            recv,
            owed: (0, BTreeMap::new()),
            decoder: hpack::Decoder::new(table),
            encoder: hpack::Encoder::new(table, most_table),
        }
    }

    /// What this side says first (§3.4, §6.5, §6.9.2): as the client, the preface's octets and
    /// SETTINGS_ENABLE_PUSH 0; its SETTINGS_INITIAL_WINDOW_SIZE where it holds other than the
    /// initial window on a stream, then a WINDOW_UPDATE growing the connection's from its initial
    /// 65,535 to what it holds, which only that frame can.
    pub fn preface(&self, end: End, out: &mut Vec<u8>) {
        let mut settings = Vec::new();
        if end == End::Client {
            out.extend_from_slice(PREFACE);
            settings.extend_from_slice(&0x2u16.to_be_bytes());
            settings.extend_from_slice(&0u32.to_be_bytes());
        }
        if self.holds.stream != INITIAL_WINDOW {
            settings.extend_from_slice(&0x4u16.to_be_bytes());
            settings.extend_from_slice(&self.holds.stream.to_be_bytes());
        }
        put(
            out,
            Head {
                length: settings.len() as u32,
                kind: SETTINGS,
                flags: 0,
                stream: 0,
            },
            &settings,
        );
        if let Some(by) = self
            .holds
            .connection
            .checked_sub(INITIAL_WINDOW)
            .filter(|&by| by > 0)
        {
            put(
                out,
                Head {
                    length: 4,
                    kind: WINDOW_UPDATE,
                    flags: 0,
                    stream: 0,
                },
                &by.to_be_bytes(),
            );
        }
    }

    /// The peer's SETTINGS frame (§6.5): an ACK of this side's puts its window in force; else its parameters
    /// applied to what this side sends (each stream's window, §6.9.2; the encoder's table, RFC
    /// 7541 §4.2; frames' length) and its ACK written onto `out` (§6.5.3).
    pub fn settings(&mut self, head: Head, payload: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        if head.stream != 0 {
            return Err(connection(Code::Protocol, "a SETTINGS frame on a stream"));
        }
        if head.flags & ACK != 0 {
            if !payload.is_empty() {
                return Err(connection(Code::FrameSize, "a SETTINGS ACK with a payload"));
            }
            return self.recv.set_initial(self.holds.stream);
        }
        let mut peer = self.peer;
        peer.apply(payload)?;
        self.send.set_initial(peer.initial_window_size)?;
        if peer.header_table_size != self.peer.header_table_size {
            self.encoder.set_limit(peer.header_table_size as usize);
        }
        self.peer = peer;
        put(
            out,
            Head {
                length: 0,
                kind: SETTINGS,
                flags: ACK,
                stream: 0,
            },
            &[],
        );
        Ok(())
    }

    /// The peer's WINDOW_UPDATE (§6.9): four octets, an increment of at least one.
    pub fn window_update(&mut self, head: Head, payload: &[u8]) -> Result<(), Error> {
        let &[a, b, c, d] = payload else {
            return Err(connection(Code::FrameSize, "a WINDOW_UPDATE not of four octets"));
        };
        let by = u32::from_be_bytes([a, b, c, d]) & MOST_STREAM;
        if by == 0 {
            return Err(Error {
                scope: if head.stream == 0 {
                    Scope::Connection
                } else {
                    Scope::Stream(head.stream)
                },
                code: Code::Protocol,
                why: "a WINDOW_UPDATE of no increment",
            });
        }
        self.send.grow(head.stream, by)
    }

    /// How much DATA this side may send on `stream` in one frame now: no more than either
    /// window's room, nor the peer's SETTINGS_MAX_FRAME_SIZE (§4.2, §6.9.1).
    pub fn sendable(&self, stream: u32) -> usize {
        let room = self.send.connection.min(self.send.stream(stream)).max(0);
        usize::try_from(room)
            .unwrap_or(0)
            .min(self.peer.max_frame_size as usize)
    }

    /// DATA frames on `stream` onto `out`, as much of `bytes` as the peer's windows let go now;
    /// how much went. The rest waits for the peer's WINDOW_UPDATE. END_STREAM goes with the last
    /// of `bytes` where `end`, on an empty frame where none are left, which no window holds back
    /// (§6.9.1).
    pub fn send_data(&mut self, stream: u32, bytes: &[u8], end: bool, out: &mut Vec<u8>) -> usize {
        let mut sent = 0;
        loop {
            let rest = bytes.get(sent..).unwrap_or_default();
            let n = self.sendable(stream).min(rest.len());
            if n == 0 && !(rest.is_empty() && end) {
                return sent;
            }
            let last = n == rest.len() && end;
            let flags = if last { END_STREAM } else { 0 };
            put(
                out,
                Head {
                    length: n as u32,
                    kind: DATA,
                    flags,
                    stream,
                },
                rest.get(..n).unwrap_or_default(),
            );
            self.send.take(stream, n);
            sent += n;
            if last || sent == bytes.len() {
                return sent;
            }
        }
    }

    /// The peer's DATA frame, its whole payload counted, padding too (§6.9.1): past what this
    /// side granted is FLOW_CONTROL_ERROR, the connection's or the stream's.
    pub fn received(&mut self, head: Head) -> Result<(), Error> {
        let n = i64::from(head.length);
        if n > self.recv.connection {
            return Err(connection(Code::FlowControl, "DATA past the connection's window"));
        }
        if n > self.recv.stream(head.stream) {
            return Err(Error {
                scope: Scope::Stream(head.stream),
                code: Code::FlowControl,
                why: "DATA past the stream's window",
            });
        }
        self.recv.take(head.stream, head.length as usize);
        Ok(())
    }

    /// `n` of the peer's octets on `stream` passed on (or its padding, which goes nowhere), so
    /// their room is the peer's again. Given back in WINDOW_UPDATEs onto `out` once a frame's
    /// worth or half the window is owed, whichever is less, as RFC 9113 §6.9.1 advises after RFC
    /// 1122 §4.2.3.3, so that the peer always has room and no update is of a few octets.
    pub fn released(&mut self, stream: u32, n: usize, out: &mut Vec<u8>) {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        let frame = self.peer_frame();
        let owed = self.owed.1.entry(stream).or_insert(0);
        *owed = owed.saturating_add(n);
        if *owed >= frame.min(self.holds.stream / 2) {
            let by = std::mem::take(owed);
            let _ = self.recv.grow(stream, by);
            put(
                out,
                Head {
                    length: 4,
                    kind: WINDOW_UPDATE,
                    flags: 0,
                    stream,
                },
                &by.to_be_bytes(),
            );
        }
        self.owed.0 = self.owed.0.saturating_add(n);
        if self.owed.0 >= frame.min(self.holds.connection / 2) {
            let by = std::mem::take(&mut self.owed.0);
            let _ = self.recv.grow(0, by);
            put(
                out,
                Head {
                    length: 4,
                    kind: WINDOW_UPDATE,
                    flags: 0,
                    stream: 0,
                },
                &by.to_be_bytes(),
            );
        }
    }

    /// The most a frame of the peer's carries: this side's SETTINGS_MAX_FRAME_SIZE, which it
    /// leaves at its initial value (§6.5.2).
    fn peer_frame(&self) -> u32 {
        Settings::INITIAL.max_frame_size
    }

    /// A stream closed: its windows and owed credit let go.
    pub fn close(&mut self, stream: u32) {
        self.send.streams.remove(&stream);
        self.recv.streams.remove(&stream);
        self.owed.1.remove(&stream);
    }
}

fn put(out: &mut Vec<u8>, head: Head, payload: &[u8]) {
    head.write(out);
    out.extend_from_slice(payload);
}

/// A DATA frame's data, its padding taken off (§6.1): padding of the whole payload or more is a
/// connection error PROTOCOL_ERROR.
pub fn data(head: Head, payload: &[u8]) -> Result<&[u8], Error> {
    if head.flags & PADDED == 0 {
        return Ok(payload);
    }
    let (&pad, rest) = payload
        .split_first()
        .ok_or(connection(Code::FrameSize, "a padded DATA frame of no length"))?;
    rest.len()
        .checked_sub(usize::from(pad))
        .and_then(|end| rest.get(..end))
        .ok_or(connection(Code::Protocol, "padding past a DATA frame's data"))
}

/// A RST_STREAM ending `stream` with `code` (§6.4), onto `out`.
pub fn rst_stream(stream: u32, code: u32, out: &mut Vec<u8>) {
    let head = Head {
        length: 4,
        kind: RST_STREAM,
        flags: 0,
        stream,
    };
    put(out, head, &code.to_be_bytes());
}

/// A GOAWAY naming `last`, the last stream of the peer's this side processed or may, and `code`
/// (§6.8), onto `out`.
pub fn goaway(last: u32, code: u32, out: &mut Vec<u8>) {
    let head = Head {
        length: 8,
        kind: GOAWAY,
        flags: 0,
        stream: 0,
    };
    put(out, head, &[(last & MOST_STREAM).to_be_bytes(), code.to_be_bytes()].concat());
}

/// A PING's ACK, its payload as it came (§6.7), onto `out`.
pub fn ping_ack(payload: &[u8], out: &mut Vec<u8>) {
    let head = Head {
        length: 8,
        kind: PING,
        flags: ACK,
        stream: 0,
    };
    put(out, head, payload);
}

/// A frame payload's first four octets as a number: RST_STREAM's error code (§6.4), GOAWAY's
/// last stream (its reserved bit cleared) and error code (§6.8).
pub fn word(payload: &[u8], at: usize) -> Option<u32> {
    let &[a, b, c, d] = payload.get(at..at.checked_add(4)?)? else {
        return None;
    };
    Some(u32::from_be_bytes([a, b, c, d]))
}

/// A field block as its frames come (§4.3, §6.2, §6.10): a HEADERS frame's fragment, then its
/// CONTINUATIONs, on its stream alone, until END_HEADERS. Wiped as it drops.
#[derive(Debug)]
pub struct Block {
    pub stream: u32,
    pub end_stream: bool,
    fragment: Zeroizing<Vec<u8>>,
}

impl Block {
    /// A HEADERS frame's fragment, its padding and priority taken off (§6.2); with whether the
    /// block is whole.
    pub fn headers(head: Head, payload: &[u8]) -> Result<(Block, bool), Error> {
        if head.stream == 0 {
            return Err(connection(Code::Protocol, "a HEADERS frame on stream 0"));
        }
        let mut fragment = payload;
        if head.flags & PADDED != 0 {
            let (&pad, rest) = fragment
                .split_first()
                .ok_or(connection(Code::FrameSize, "a padded HEADERS frame of no length"))?;
            fragment = rest
                .len()
                .checked_sub(usize::from(pad))
                .and_then(|end| rest.get(..end))
                .ok_or(connection(
                    Code::Protocol,
                    "padding past a HEADERS frame's fragment",
                ))?;
        }
        if head.flags & PRIORITY != 0 {
            fragment = fragment.get(5..).ok_or(connection(
                Code::FrameSize,
                "a HEADERS frame too short for its priority",
            ))?;
        }
        let block = Block {
            stream: head.stream,
            end_stream: head.flags & END_STREAM != 0,
            fragment: Zeroizing::new(fragment.to_vec()),
        };
        Ok((block, head.flags & END_HEADERS != 0))
    }

    /// The octets of its fragments held.
    pub fn held(&self) -> usize {
        self.fragment.len()
    }

    /// A frame after an unfinished block's: a CONTINUATION on its stream (§6.10), else a
    /// connection error PROTOCOL_ERROR; past `most` octets held, ENHANCE_YOUR_CALM (§10.5), as a
    /// block not decoded leaves the connection's compression unknown (§4.3). Whether it is whole.
    pub fn continued(&mut self, head: Head, payload: &[u8], most: usize) -> Result<bool, Error> {
        if head.kind != CONTINUATION || head.stream != self.stream {
            return Err(connection(Code::Protocol, "a field block interrupted"));
        }
        if self.fragment.len().saturating_add(payload.len()) > most {
            return Err(connection(
                Code::EnhanceYourCalm,
                "a field block past what the connection holds",
            ));
        }
        self.fragment.extend_from_slice(payload);
        Ok(head.flags & END_HEADERS != 0)
    }

    /// Its fields, decoded by its hop's decoder (any error a COMPRESSION_ERROR, §4.3), none kept
    /// past `most` octets as RFC 9113 §6.5.2 counts them.
    pub fn decode(&self, decoder: &mut hpack::Decoder, most: usize) -> Result<hpack::Decoded, Error> {
        decoder
            .decode(&self.fragment, most)
            .map_err(|_| connection(Code::Compression, "a field block not HPACK"))
    }
}

/// HEADERS then CONTINUATION frames carrying `block` on `stream` onto `out`, none longer than
/// `most`, the peer's SETTINGS_MAX_FRAME_SIZE (§4.2, §4.3).
pub fn headers(stream: u32, block: &[u8], end_stream: bool, most: u32, out: &mut Vec<u8>) {
    let most = (most as usize).max(1);
    let mut pieces = block.chunks(most).peekable();
    let mut kind = HEADERS;
    let mut flags = if end_stream { END_STREAM } else { 0 };
    loop {
        let piece = pieces.next().unwrap_or_default();
        if pieces.peek().is_none() {
            flags |= END_HEADERS;
        }
        put(
            out,
            Head {
                length: piece.len() as u32,
                kind,
                flags,
                stream,
            },
            piece,
        );
        if flags & END_HEADERS != 0 {
            return;
        }
        (kind, flags) = (CONTINUATION, 0);
    }
}

/// What the injector does with a request's field block (D242), as `credential::decide` says.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// It goes on to the host, each fill's credential filled in where its placeholder stands.
    Go(Go),
    /// Its host is not the SNI: answered 421 on its stream alone (RFC 9110 §7.4, f44).
    Misdirected { host: String },
    /// Malformed (RFC 9113 §8.1.1): a stream error PROTOCOL_ERROR, never sent on.
    Malformed(&'static str),
}

/// A request to go on: its fields as they came, its host, and its fills.
#[derive(Debug, PartialEq, Eq)]
pub struct Go {
    pub fields: Vec<Field>,
    pub host: String,
    pub fills: Vec<Filled>,
}

/// A credential to be filled in: the field, where its placeholder lies in its value, and the
/// credential `decide` named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
    pub field: usize,
    pub token: Range<usize>,
    pub reference: String,
}

/// The request pseudo-header fields (§8.3.1), and RFC 8441's `:protocol` (§4).
const PSEUDO: [&[u8]; 5] = [b":method", b":scheme", b":authority", b":path", b":protocol"];
/// The fields HTTP/2 has no use of (§8.2.2).
const CONNECTION_SPECIFIC: [&[u8]; 5] = [
    b"connection",
    b"proxy-connection",
    b"keep-alive",
    b"transfer-encoding",
    b"upgrade",
];

/// A request's decoded `fields`, held to RFC 9113 §8.2 and §8.3.1 as an origin holds them (a
/// gateway is one, RFC 9110 §3.7), and what `decide` says of each credential field's token for
/// `grants` on a flow whose TLS named `sni`.
pub fn request(fields: Vec<Field>, grants: &[CredentialGrant], sni: &str) -> Request {
    if let Err(why) = well_formed(&fields) {
        return Request::Malformed(why);
    }
    let one = |name: &[u8]| {
        let mut all = fields.iter().filter(move |f| f.name == name);
        (all.next(), all.next())
    };
    let host = match (one(b":authority"), one(b"host")) {
        ((Some(_), Some(_)), _) => return Request::Malformed("two :authority fields"),
        (_, (Some(_), Some(_))) => return Request::Malformed("two Host fields"),
        ((Some(a), None), (Some(h), None)) if !a.value.eq_ignore_ascii_case(&h.value) => {
            return Request::Malformed("a Host that is not the :authority");
        }
        ((Some(f), None), _) | (_, (Some(f), None)) => f,
        _ => return Request::Malformed("a request that names no host"),
    };
    let Some(host) = std::str::from_utf8(&host.value).ok().and_then(authority_host) else {
        return Request::Malformed("a request's host is no host");
    };
    if credential::decide(grants, sni, &host, None) == Fill::Misdirected {
        return Request::Misdirected { host };
    }
    let mut fills = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let Some(token) = token_in(&field.name, &field.value) else {
            continue;
        };
        let Some(placeholder) = field
            .value
            .get(token.clone())
            .and_then(|t| std::str::from_utf8(t).ok())
        else {
            continue;
        };
        if let Fill::With(reference) = credential::decide(grants, sni, &host, Some(placeholder)) {
            fills.push(Filled {
                field: i,
                token,
                reference,
            });
        }
    }
    Request::Go(Go { fields, host, fills })
}

/// RFC 9113 §8.2.1's least validation, §8.2.2's connection-specific fields, and §8.3.1's
/// pseudo-header fields: known ones, each once, before every other field.
fn well_formed(fields: &[Field]) -> Result<(), &'static str> {
    let mut regular = false;
    let mut seen: Vec<&[u8]> = Vec::new();
    for f in fields {
        let pseudo = f.name.first() == Some(&b':');
        let name = if pseudo {
            f.name.get(1..).unwrap_or_default()
        } else {
            &f.name[..]
        };
        if name.is_empty()
            || name
                .iter()
                .any(|&b| b <= 0x20 || b.is_ascii_uppercase() || b >= 0x7f || b == b':')
        {
            return Err("a field name HTTP/2 forbids");
        }
        if f.value.iter().any(|&b| matches!(b, 0 | b'\n' | b'\r'))
            || f.value.first().is_some_and(|&b| b == b' ' || b == b'\t')
            || f.value.last().is_some_and(|&b| b == b' ' || b == b'\t')
        {
            return Err("a field value HTTP/2 forbids");
        }
        if pseudo {
            if regular || !PSEUDO.contains(&&f.name[..]) || seen.contains(&&f.name[..]) {
                return Err("a pseudo-header field out of place, unknown or twice");
            }
            seen.push(&f.name);
        } else {
            regular = true;
            if CONNECTION_SPECIFIC.contains(&&f.name[..]) || (f.name == b"te" && &f.value[..] != b"trailers")
            {
                return Err("a connection-specific field");
            }
        }
    }
    let connect = fields
        .iter()
        .any(|f| f.name == b":method" && &f.value[..] == b"CONNECT");
    let has = |n: &[u8]| seen.contains(&n);
    let complete = has(b":method")
        && if connect && !has(b":protocol") {
            !has(b":scheme") && !has(b":path") && has(b":authority")
        } else {
            has(b":scheme") && has(b":path")
        };
    complete
        .then_some(())
        .ok_or("a request without its pseudo-header fields")
}

impl Go {
    /// The references of the credentials to lease, once each.
    pub fn references(&self) -> Vec<&str> {
        let mut all: Vec<&str> = Vec::new();
        for f in &self.fills {
            if !all.contains(&f.reference.as_str()) {
                all.push(&f.reference);
            }
        }
        all
    }

    /// The block for the host's hop: each fill's value (`value` gives a held lease's) in its
    /// placeholder's place, every credential field never indexed (RFC 7541 §7.1.3, f42), every
    /// other field as it came, encoded by the host's hop's own encoder; wiped as it drops. None
    /// where a value is not held, the encoder untouched, so that the caller can ask again.
    pub fn encode<'v>(
        &self,
        encoder: &mut hpack::Encoder,
        value: impl Fn(&str) -> Option<&'v [u8]>,
    ) -> Option<Zeroizing<Vec<u8>>> {
        let mut fields = self.fields.clone();
        for fill in &self.fills {
            let value = value(&fill.reference)?;
            let field = fields.get_mut(fill.field)?;
            let mut filled =
                Zeroizing::new(Vec::with_capacity(field.value.len().saturating_add(value.len())));
            filled.extend_from_slice(field.value.get(..fill.token.start)?);
            filled.extend_from_slice(value);
            filled.extend_from_slice(field.value.get(fill.token.end..)?);
            field.value = filled;
        }
        for field in &mut fields {
            // The fields `token_in` reads a credential from, filled or not.
            if matches!(&field.name[..], b"authorization" | b"x-api-key") {
                field.indexing = Indexing::Never;
            }
        }
        let mut block = Zeroizing::new(Vec::new());
        encoder.encode(&fields, &mut block);
        Some(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: &str = "api.anthropic.com";
    const OPENAI: &str = "api.openai.com";
    const VALUE: &[u8] = b"sk-ant-api03-the-real-value";
    const HOLDS: Holds = Holds {
        stream: INITIAL_WINDOW,
        connection: INITIAL_WINDOW,
    };

    fn placeholder(c: char) -> String {
        format!("kiln-placeholder-{}", c.to_string().repeat(64))
    }

    fn grants() -> Vec<CredentialGrant> {
        vec![
            CredentialGrant {
                reference: "anthropic/key".into(),
                toward: vec![ANTHROPIC.into()],
                placeholder: placeholder('a'),
            },
            CredentialGrant {
                reference: "openai/key".into(),
                toward: vec![OPENAI.into()],
                placeholder: placeholder('b'),
            },
        ]
    }

    /// A request as a guest sends it, its placeholder indexed, as RFC 7541 §7.1.3 lets a client.
    fn request_fields(authority: &str, authorization: &str, extra: &[(&str, &str)]) -> Vec<Field> {
        let mut fields = vec![
            Field::new(b":method", b"POST", Indexing::Incremental),
            Field::new(b":scheme", b"https", Indexing::Incremental),
            Field::new(b":authority", authority.as_bytes(), Indexing::Incremental),
            Field::new(b":path", b"/v1/messages", Indexing::Incremental),
            Field::new(b"authorization", authorization.as_bytes(), Indexing::Incremental),
        ];
        fields.extend(
            extra
                .iter()
                .map(|(n, v)| Field::new(n.as_bytes(), v.as_bytes(), Indexing::Incremental)),
        );
        fields
    }

    fn bearer(token: &[u8]) -> Vec<u8> {
        [b"Bearer ", token].concat()
    }

    /// The guest's side: its own encoder's block for `fields`, and the frames carrying it.
    fn guest_sends(encoder: &mut hpack::Encoder, stream: u32, fields: &[Field]) -> (Vec<u8>, Vec<u8>) {
        let mut block = Vec::new();
        encoder.encode(fields, &mut block);
        let mut wire = Vec::new();
        headers(stream, &block, false, Settings::INITIAL.max_frame_size, &mut wire);
        (block, wire)
    }

    /// A field block read from its frames and decoded by `decoder`.
    fn read(decoder: &mut hpack::Decoder, wire: &[u8]) -> Vec<Field> {
        let most = Settings::INITIAL.max_frame_size;
        let (head, payload) = frame(wire, most).unwrap().unwrap();
        let (mut block, mut whole) = Block::headers(head, payload).unwrap();
        let mut at = HEAD + payload.len();
        while !whole {
            let (head, payload) = frame(&wire[at..], most).unwrap().unwrap();
            whole = block.continued(head, payload, usize::MAX).unwrap();
            at += HEAD + payload.len();
        }
        assert_eq!(at, wire.len(), "one block, nothing after it");
        match block.decode(decoder, usize::MAX).unwrap() {
            hpack::Decoded::Fields(fields) => fields,
            other => panic!("{other:?}"),
        }
    }

    fn pairs(fields: &[Field]) -> Vec<(Vec<u8>, Vec<u8>)> {
        fields
            .iter()
            .map(|f| (f.name.clone(), f.value.to_vec()))
            .collect()
    }

    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn go(request: Request) -> Go {
        match request {
            Request::Go(go) => go,
            other => panic!("{other:?}"),
        }
    }

    fn head(kind: u8, stream: u32, length: u32) -> Head {
        Head {
            length,
            kind,
            flags: 0,
            stream,
        }
    }

    fn settings(params: &[(u16, u32)]) -> Vec<u8> {
        params
            .iter()
            .flat_map(|(i, v)| [&i.to_be_bytes()[..], &v.to_be_bytes()[..]].concat())
            .collect()
    }

    /// Each frame's type, flags, stream and length.
    fn frames(mut wire: &[u8]) -> Vec<(u8, u8, u32, u32)> {
        let mut all = Vec::new();
        while let Some((h, payload)) = frame(wire, *FRAME_SIZES.end()).unwrap() {
            all.push((h.kind, h.flags, h.stream, h.length));
            wire = &wire[HEAD + payload.len()..];
        }
        all
    }

    #[test]
    fn a_placeholder_in_a_header_block_comes_out_filled_and_never_indexed() {
        let grants = grants();
        let mut guest = hpack::Encoder::new(4096, 4096);
        let mut guest_hop = Hop::new(HOLDS, 4096);
        let mut host_hop = Hop::new(HOLDS, 4096);
        let mut host = hpack::Decoder::new(4096);
        // A field past a frame's 16,384 octets, so that each side's block runs into CONTINUATIONs.
        let large = "v".repeat(20_000);
        let sent = request_fields(
            ANTHROPIC,
            &format!("Bearer {}", placeholder('a')),
            &[("content-type", "application/json"), ("x-large", &large)],
        );
        // Twice on one connection: the second block finds the value in no table of either hop.
        for stream in [1, 3] {
            let (_, wire) = guest_sends(&mut guest, stream, &sent);
            let go = go(request(read(&mut guest_hop.decoder, &wire), &grants, ANTHROPIC));
            assert_eq!(go.references(), ["anthropic/key"]);
            let block = go
                .encode(&mut host_hop.encoder, |r| (r == "anthropic/key").then_some(VALUE))
                .unwrap();
            let mut out = Vec::new();
            headers(stream, &block, false, host_hop.peer.max_frame_size, &mut out);
            assert!(frames(&out).len() > 1, "HEADERS and CONTINUATION");
            let arrived = read(&mut host, &out);
            let mut expected = pairs(&sent);
            expected[4].1 = bearer(VALUE);
            assert_eq!(pairs(&arrived), expected, "stream {stream}");
            assert_eq!(arrived[4].indexing, Indexing::Never, "stream {stream}");
            assert!(!holds(&out, placeholder('a').as_bytes()));
        }
    }

    #[test]
    fn an_ungranted_hosts_block_passes_as_it_came() {
        // A flow to api.openai.com, whose TLS the injector ends as a grant names it, carrying the
        // placeholder of a credential granted toward api.anthropic.com alone.
        let sent = request_fields(
            OPENAI,
            &format!("Bearer {}", placeholder('a')),
            &[("x-trace", "1")],
        );
        let mut guest = hpack::Encoder::new(4096, 4096);
        let mut guest_hop = Hop::new(HOLDS, 4096);
        let mut host_hop = Hop::new(HOLDS, 4096);
        let (_, wire) = guest_sends(&mut guest, 1, &sent);
        let go = go(request(read(&mut guest_hop.decoder, &wire), &grants(), OPENAI));
        assert!(go.fills.is_empty());
        let block = go
            .encode(&mut host_hop.encoder, |r| -> Option<&'static [u8]> {
                panic!("no lease of {r} asked")
            })
            .unwrap();
        let mut out = Vec::new();
        headers(1, &block, false, host_hop.peer.max_frame_size, &mut out);
        assert_eq!(pairs(&read(&mut hpack::Decoder::new(4096), &out)), pairs(&sent));
    }

    #[test]
    fn a_block_for_a_host_not_the_sni_is_misdirected() {
        let sent = request_fields(ANTHROPIC, &format!("Bearer {}", placeholder('a')), &[]);
        assert_eq!(
            request(sent.clone(), &grants(), OPENAI),
            Request::Misdirected {
                host: ANTHROPIC.into()
            }
        );
        let mut host = sent.clone();
        host.push(Field::new(b"host", b"evil.example", Indexing::Incremental));
        assert!(matches!(
            request(host, &grants(), ANTHROPIC),
            Request::Malformed(_)
        ));
        let mut upper = sent;
        upper.push(Field::new(b"X-Up", b"1", Indexing::Incremental));
        assert!(matches!(
            request(upper, &grants(), ANTHROPIC),
            Request::Malformed(_)
        ));
    }

    #[test]
    fn each_hops_dynamic_table_stays_its_own_across_blocks() {
        let grants = grants();
        let mut guest = hpack::Encoder::new(4096, 4096);
        let mut guest_hop = Hop::new(HOLDS, 4096);
        let mut host_hop = Hop::new(HOLDS, 4096);
        // The host keeps a table of 128 octets where the guest's encoder keeps 4,096.
        let mut host = hpack::Decoder::new(4096);
        let mut ack = Vec::new();
        let params = settings(&[(0x1, 128)]);
        host_hop
            .settings(head(SETTINGS, 0, params.len() as u32), &params, &mut ack)
            .unwrap();
        host.set_limit(128);
        let session = "s".repeat(40);
        for n in 0..3u32 {
            let sent = request_fields(
                ANTHROPIC,
                &format!("Bearer {}", placeholder('a')),
                &[("x-session", &session), ("x-request", &n.to_string())],
            );
            let (guest_block, wire) = guest_sends(&mut guest, 1 + 2 * n, &sent);
            let got = read(&mut guest_hop.decoder, &wire);
            assert_eq!(
                pairs(&got),
                pairs(&sent),
                "the guest's hop decodes block {n} with its own table"
            );
            let block = go(request(got, &grants, ANTHROPIC))
                .encode(&mut host_hop.encoder, |_| Some(VALUE))
                .unwrap();
            let arrived = match host.decode(&block, usize::MAX).unwrap() {
                hpack::Decoded::Fields(f) => f,
                other => panic!("{other:?}"),
            };
            let mut expected = pairs(&sent);
            expected[4].1 = bearer(VALUE);
            assert_eq!(
                pairs(&arrived),
                expected,
                "the host's hop decodes block {n} with its own table"
            );
            if n > 0 {
                assert_ne!(
                    block[..],
                    guest_block[..],
                    "block {n} re-encoded, never passed as it came"
                );
            }
        }
    }

    #[test]
    fn flow_control_holds_a_sender_past_its_peers_window() {
        let mut hop = Hop::new(HOLDS, 4096);
        let mut out = Vec::new();
        let set = |hop: &mut Hop, params: &[(u16, u32)]| {
            let p = settings(params);
            hop.settings(head(SETTINGS, 0, p.len() as u32), &p, &mut Vec::new())
        };
        // The peer grants each stream 10 octets; its SETTINGS answered with an ACK.
        let p = settings(&[(0x4, 10)]);
        hop.settings(head(SETTINGS, 0, p.len() as u32), &p, &mut out)
            .unwrap();
        assert_eq!(frames(&out), [(SETTINGS, ACK, 0, 0)]);
        out.clear();
        let body = [7u8; 25];
        assert_eq!(hop.send_data(1, &body, true, &mut out), 10);
        assert_eq!(
            frames(&out),
            [(DATA, 0, 1, 10)],
            "no END_STREAM before the last octet"
        );
        out.clear();
        assert_eq!(
            hop.send_data(1, &body[10..], true, &mut out),
            0,
            "held past the stream's window"
        );
        assert!(out.is_empty());
        hop.window_update(head(WINDOW_UPDATE, 1, 4), &7u32.to_be_bytes())
            .unwrap();
        assert_eq!(hop.send_data(1, &body[10..], true, &mut out), 7);
        // A lowered SETTINGS_INITIAL_WINDOW_SIZE moves the open stream's window below zero, and
        // it is held until WINDOW_UPDATEs bring it above (§6.9.2).
        set(&mut hop, &[(0x4, 0)]).unwrap();
        hop.window_update(head(WINDOW_UPDATE, 1, 4), &5u32.to_be_bytes())
            .unwrap();
        assert_eq!(hop.sendable(1), 0);
        hop.window_update(head(WINDOW_UPDATE, 1, 4), &8u32.to_be_bytes())
            .unwrap();
        out.clear();
        assert_eq!(hop.send_data(1, &body[17..], true, &mut out), 3);
        // The connection's window holds every stream: 65,535 in all, in frames no longer than
        // the peer's SETTINGS_MAX_FRAME_SIZE, whatever a stream's window says.
        set(&mut hop, &[(0x4, 1 << 20)]).unwrap();
        let big = vec![1u8; 1 << 20];
        out.clear();
        assert_eq!(hop.send_data(3, &big, false, &mut out), 65_535 - 20);
        assert!(frames(&out).iter().all(|&(_, _, _, n)| n <= 1 << 14));
        assert_eq!(
            hop.send_data(5, &big, false, &mut out),
            0,
            "the connection's window spent"
        );
        hop.window_update(head(WINDOW_UPDATE, 0, 4), &100u32.to_be_bytes())
            .unwrap();
        assert_eq!(hop.send_data(5, &big, false, &mut out), 100);
        // An empty END_STREAM goes whatever the windows hold (§6.9.1).
        out.clear();
        assert_eq!(hop.send_data(5, &[], true, &mut out), 0);
        assert_eq!(frames(&out), [(DATA, END_STREAM, 5, 0)]);
        // Refused: an increment of none; a window past 2^31-1; settings out of their ranges.
        let zero = hop
            .window_update(head(WINDOW_UPDATE, 7, 4), &0u32.to_be_bytes())
            .unwrap_err();
        assert_eq!((zero.scope, zero.code), (Scope::Stream(7), Code::Protocol));
        hop.window_update(head(WINDOW_UPDATE, 0, 4), &1u32.to_be_bytes())
            .unwrap();
        let past = hop
            .window_update(head(WINDOW_UPDATE, 0, 4), &((1u32 << 31) - 1).to_be_bytes())
            .unwrap_err();
        assert_eq!((past.scope, past.code), (Scope::Connection, Code::FlowControl));
        assert_eq!(
            set(&mut hop, &[(0x4, 1 << 31)]).unwrap_err().code,
            Code::FlowControl
        );
        assert_eq!(
            set(&mut hop, &[(0x5, (1 << 14) - 1)]).unwrap_err().code,
            Code::Protocol
        );
        assert_eq!(set(&mut hop, &[(0x5, 1 << 24)]).unwrap_err().code, Code::Protocol);
    }

    #[test]
    fn a_peer_past_this_sides_window_is_refused_and_credit_comes_back_as_its_data_goes() {
        let granted = Holds {
            stream: 40_000,
            connection: 100_000,
        };
        let mut hop = Hop::new(granted, 4096);
        let mut out = Vec::new();
        hop.preface(End::Server, &mut out);
        assert_eq!(frames(&out), [(SETTINGS, 0, 0, 6), (WINDOW_UPDATE, 0, 0, 4)]);
        assert!(holds(&out, &settings(&[(0x4, 40_000)])));
        assert!(holds(&out, &(100_000u32 - 65_535).to_be_bytes()));
        // Its window in force once the peer acknowledges it.
        let ack = Head {
            length: 0,
            kind: SETTINGS,
            flags: ACK,
            stream: 0,
        };
        hop.settings(ack, &[], &mut out).unwrap();
        let data = |stream, length| Head {
            length,
            kind: DATA,
            flags: 0,
            stream,
        };
        hop.received(data(1, 16_384)).unwrap();
        hop.received(data(1, 16_384)).unwrap();
        let past = hop.received(data(1, 16_384)).unwrap_err();
        assert_eq!((past.scope, past.code), (Scope::Stream(1), Code::FlowControl));
        // Given back once a frame's worth is owed, the stream's and the connection's.
        out.clear();
        hop.released(1, 16_383, &mut out);
        assert!(out.is_empty());
        hop.released(1, 1, &mut out);
        assert_eq!(frames(&out), [(WINDOW_UPDATE, 0, 1, 4), (WINDOW_UPDATE, 0, 0, 4)]);
        // The connection's 100,000 hold every stream: 67,232 spent and 16,384 given back.
        for stream in [1, 3, 3, 5, 7] {
            hop.received(data(stream, 16_384)).unwrap();
        }
        let past = hop.received(data(9, 16_384)).unwrap_err();
        assert_eq!((past.scope, past.code), (Scope::Connection, Code::FlowControl));
    }
}
