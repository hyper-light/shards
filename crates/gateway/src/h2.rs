//! An HTTP/2 client (RFC 9113) of one connection over a byte pipe, with prior knowledge
//! (§3.3), as grpc-go dials a frontend's stdio. Its streams open one at a time, each a
//! request and its response; SETTINGS, PING, WINDOW_UPDATE and GOAWAY are answered as
//! they come; flow control is kept both ways (§5.2, §6.9). Every frame is bounded by this
//! side's MAX_FRAME_SIZE (the default, 16384), every header block by its
//! MAX_HEADER_LIST_SIZE, every body by what the caller allows.

use std::io::{Read, Write};

use crate::hpack::{self, Field};

/// The client connection preface (§3.4).
const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// The frame size this side takes: the default, which it never raises (§6.5.2).
const MAX_FRAME: usize = 16_384;
/// The window this side gives each stream and the connection: a gRPC message's most,
/// 16 MiB (grpcclient's MaxCallRecvMsgSize), with its prefix, fits one stream's.
pub const WINDOW: u32 = 1 << 25;
/// The header lists this side takes (MAX_HEADER_LIST_SIZE): BuildKit's errors carry
/// their stacks in `grpc-status-details-bin`.
const MAX_HEADER_LIST: usize = 1 << 20;
/// A window may not pass 2^31-1 (§6.9.1).
const MAX_WINDOW: i64 = (1 << 31) - 1;

mod kind {
    pub const DATA: u8 = 0;
    pub const HEADERS: u8 = 1;
    pub const RST_STREAM: u8 = 3;
    pub const SETTINGS: u8 = 4;
    pub const PUSH_PROMISE: u8 = 5;
    pub const PING: u8 = 6;
    pub const GOAWAY: u8 = 7;
    pub const WINDOW_UPDATE: u8 = 8;
    pub const CONTINUATION: u8 = 9;
}

mod flag {
    pub const END_STREAM: u8 = 0x1;
    pub const ACK: u8 = 0x1;
    pub const END_HEADERS: u8 = 0x4;
    pub const PADDED: u8 = 0x8;
    pub const PRIORITY: u8 = 0x20;
}

/// What a connection or a stream ended with instead of its response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http2: {}", self.0)
    }
}

fn bad<T>(why: impl Into<String>) -> Result<T, Error> {
    Err(Error(why.into()))
}

fn io(e: std::io::Error) -> Error {
    Error(format!("the connection: {e}"))
}

/// A response: its headers, its body, and its trailers (empty for one whose headers
/// ended the stream, gRPC's "trailers-only").
#[derive(Debug, Default)]
pub struct Response {
    pub headers: Vec<Field>,
    pub body: Vec<u8>,
    pub trailers: Vec<Field>,
}

/// A frame: its type, flags, stream and payload.
#[derive(Debug)]
struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

/// Where a response is read up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Headers,
    Body,
    Done,
}

/// The stream in flight.
#[derive(Debug)]
struct Pending {
    id: u32,
    phase: Phase,
    response: Response,
    most: usize,
    /// What this side may still send on it.
    send_window: i64,
    /// What the peer sent on it that this side has not yet given back.
    taken: u64,
}

/// One client connection.
#[derive(Debug)]
pub struct Conn<R, W> {
    r: R,
    w: W,
    decoder: hpack::Decoder,
    next_stream: u32,
    /// The peer's MAX_FRAME_SIZE and INITIAL_WINDOW_SIZE.
    peer_frame: usize,
    peer_window: i64,
    /// What this side may send on the connection.
    send_window: i64,
    /// What the peer sent on the connection that this side has not yet given back.
    taken: u64,
    /// The last stream the peer will answer, once it has said GOAWAY.
    last_stream: Option<u32>,
}

impl<R: Read, W: Write> Conn<R, W> {
    /// A connection over `r` and `w`: the preface, this side's settings (no push, its
    /// windows, its header lists), and the connection's own window raised to [`WINDOW`].
    pub fn new(r: R, mut w: W) -> Result<Conn<R, W>, Error> {
        let mut settings = Vec::new();
        for (id, value) in [(0x2u16, 0u32), (0x4, WINDOW), (0x6, MAX_HEADER_LIST as u32)] {
            settings.extend_from_slice(&id.to_be_bytes());
            settings.extend_from_slice(&value.to_be_bytes());
        }
        let mut out = PREFACE.to_vec();
        frame_into(&mut out, kind::SETTINGS, 0, 0, &settings);
        frame_into(
            &mut out,
            kind::WINDOW_UPDATE,
            0,
            0,
            &(WINDOW - 65_535).to_be_bytes(),
        );
        w.write_all(&out).map_err(io)?;
        w.flush().map_err(io)?;
        Ok(Conn {
            r,
            w,
            decoder: hpack::Decoder::new(4096, MAX_HEADER_LIST),
            next_stream: 1,
            peer_frame: MAX_FRAME,
            peer_window: 65_535,
            send_window: 65_535,
            taken: 0,
            last_stream: None,
        })
    }

    /// One request on a stream of its own, and its response, its body at most `most`
    /// bytes.
    pub fn request(&mut self, headers: &[(&str, &str)], body: &[u8], most: usize) -> Result<Response, Error> {
        if self.last_stream.is_some() {
            return bad("the peer said GOAWAY");
        }
        let id = self.next_stream;
        self.next_stream = id
            .checked_add(2)
            .filter(|s| *s < 1 << 31)
            .ok_or_else(|| Error("no stream identifiers left".into()))?;
        let mut p = Pending {
            id,
            phase: Phase::Headers,
            response: Response::default(),
            most,
            send_window: self.peer_window,
            taken: 0,
        };
        // Its headers, in as many frames as the peer's frame size takes (§6.10).
        let block = hpack::encode(headers);
        let ends = if body.is_empty() { flag::END_STREAM } else { 0 };
        let mut out = Vec::new();
        let mut chunks = block.chunks(self.peer_frame.max(1)).peekable();
        let mut first = true;
        loop {
            let chunk = chunks.next().unwrap_or_default();
            let end = if chunks.peek().is_none() {
                flag::END_HEADERS
            } else {
                0
            };
            if first {
                frame_into(&mut out, kind::HEADERS, end | ends, id, chunk);
                first = false;
            } else {
                frame_into(&mut out, kind::CONTINUATION, end, id, chunk);
            }
            if end != 0 {
                break;
            }
        }
        self.w.write_all(&out).map_err(io)?;
        // Its body, as the windows let it go.
        let mut sent = 0usize;
        while sent < body.len() {
            let room = self.send_window.min(p.send_window);
            if room <= 0 {
                self.w.flush().map_err(io)?;
                self.next(&mut p)?;
                if p.phase == Phase::Done {
                    return Ok(p.response);
                }
                continue;
            }
            let n = usize::try_from(room)
                .unwrap_or(usize::MAX)
                .min(self.peer_frame)
                .min(body.len() - sent);
            let end = if sent + n == body.len() {
                flag::END_STREAM
            } else {
                0
            };
            let mut f = Vec::with_capacity(9 + n);
            frame_into(
                &mut f,
                kind::DATA,
                end,
                id,
                body.get(sent..sent + n).unwrap_or_default(),
            );
            self.w.write_all(&f).map_err(io)?;
            sent += n;
            let n = i64::try_from(n).unwrap_or(i64::MAX);
            self.send_window -= n;
            p.send_window -= n;
        }
        self.w.flush().map_err(io)?;
        // Its response.
        while p.phase != Phase::Done {
            self.next(&mut p)?;
        }
        Ok(p.response)
    }

    /// The next frame, applied: the connection's own, or the stream's, or another's,
    /// whose headers still change the table (§4.3) and whose data still takes the
    /// connection's window.
    fn next(&mut self, p: &mut Pending) -> Result<(), Error> {
        let f = self.read_frame()?;
        match f.kind {
            kind::SETTINGS => self.settings(&f, p),
            kind::PING => self.ping(&f),
            kind::WINDOW_UPDATE => self.window_update(&f, p),
            kind::GOAWAY => {
                let last = f
                    .payload
                    .get(..4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(|b| u32::from_be_bytes(b) & 0x7fff_ffff)
                    .ok_or_else(|| Error("a GOAWAY shorter than its fields".into()))?;
                self.last_stream = Some(last);
                if p.id > last {
                    return bad("the peer said GOAWAY before this stream");
                }
                Ok(())
            }
            kind::PUSH_PROMISE => bad("a PUSH_PROMISE, which this side did not allow"),
            kind::CONTINUATION => bad("a CONTINUATION after a complete header block"),
            kind::HEADERS if f.stream == p.id => {
                let ends = f.flags & flag::END_STREAM != 0;
                let fields = self.header_block(f)?;
                match p.phase {
                    Phase::Headers => p.response.headers = fields,
                    Phase::Body if ends => p.response.trailers = fields,
                    _ => return bad("headers after the trailers"),
                }
                p.phase = if ends { Phase::Done } else { Phase::Body };
                Ok(())
            }
            kind::HEADERS if f.stream != 0 => self.header_block(f).map(|_| ()),
            kind::DATA if f.stream == p.id => {
                if p.phase != Phase::Body {
                    return bad("data before the response's headers");
                }
                let ends = f.flags & flag::END_STREAM != 0;
                let data = unpad(&f)?;
                if p.response.body.len().saturating_add(data.len()) > p.most {
                    return bad(format!("a response larger than {} bytes", p.most));
                }
                p.response.body.extend_from_slice(data);
                self.give_back(p, f.payload.len(), !ends)?;
                if ends {
                    p.phase = Phase::Done;
                }
                Ok(())
            }
            kind::DATA if f.stream != 0 => self.give_back(p, f.payload.len(), false),
            kind::RST_STREAM if f.stream == p.id => {
                let code = f
                    .payload
                    .get(..4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map_or(0, u32::from_be_bytes);
                bad(format!("the stream was reset ({})", error_code(code)))
            }
            kind::DATA | kind::HEADERS | kind::RST_STREAM if f.stream == 0 => {
                bad("a stream's frame on the connection")
            }
            // PRIORITY, a closed stream's RST_STREAM, and types this side does not know
            // are passed over (§4.1, §5.5).
            _ => Ok(()),
        }
    }

    fn settings(&mut self, f: &Frame, p: &mut Pending) -> Result<(), Error> {
        if f.stream != 0 {
            return bad("SETTINGS on a stream");
        }
        if f.flags & flag::ACK != 0 {
            if !f.payload.is_empty() {
                return bad("a SETTINGS acknowledgement with settings");
            }
            return Ok(());
        }
        let (settings, rest) = f.payload.as_chunks::<6>();
        if !rest.is_empty() {
            return bad("SETTINGS of a length not a multiple of six");
        }
        for &[a, b, c, d, e, g] in settings {
            let value = u32::from_be_bytes([c, d, e, g]);
            match u16::from_be_bytes([a, b]) {
                // §6.9.2: a change of the initial window changes the open stream's too.
                0x4 => {
                    if i64::from(value) > MAX_WINDOW {
                        return bad("an initial window past 2^31-1");
                    }
                    let delta = i64::from(value) - self.peer_window;
                    self.peer_window = i64::from(value);
                    p.send_window = p.send_window.saturating_add(delta);
                    if p.send_window > MAX_WINDOW {
                        return bad("a stream window past 2^31-1");
                    }
                }
                0x5 => {
                    if !(16_384..=16_777_215).contains(&value) {
                        return bad("a maximum frame size out of range");
                    }
                    self.peer_frame = usize::try_from(value).unwrap_or(MAX_FRAME);
                }
                _ => {}
            }
        }
        let mut ack = Vec::new();
        frame_into(&mut ack, kind::SETTINGS, flag::ACK, 0, &[]);
        self.send(&ack)
    }

    fn ping(&mut self, f: &Frame) -> Result<(), Error> {
        if f.payload.len() != 8 || f.stream != 0 {
            return bad("a PING not of eight bytes on the connection");
        }
        if f.flags & flag::ACK != 0 {
            return Ok(());
        }
        let mut ack = Vec::new();
        frame_into(&mut ack, kind::PING, flag::ACK, 0, &f.payload);
        self.send(&ack)
    }

    fn window_update(&mut self, f: &Frame, p: &mut Pending) -> Result<(), Error> {
        let inc = f
            .payload
            .as_slice()
            .try_into()
            .map(|b: [u8; 4]| i64::from(u32::from_be_bytes(b) & 0x7fff_ffff))
            .map_err(|_| Error("a WINDOW_UPDATE not of four bytes".into()))?;
        if inc == 0 {
            return bad("a WINDOW_UPDATE of nothing");
        }
        let window = match f.stream {
            0 => &mut self.send_window,
            s if s == p.id => &mut p.send_window,
            _ => return Ok(()),
        };
        *window = window.saturating_add(inc);
        if *window > MAX_WINDOW {
            return bad("a window past 2^31-1");
        }
        Ok(())
    }

    /// Credits `len` bytes the peer sent back to its windows, each once half of it is
    /// taken: the connection's always, the stream's while it goes on.
    fn give_back(&mut self, p: &mut Pending, len: usize, stream: bool) -> Result<(), Error> {
        let len = u64::try_from(len).unwrap_or(u64::MAX);
        if len == 0 {
            return Ok(());
        }
        let half = u64::from(WINDOW / 2);
        let mut out = Vec::new();
        self.taken = self.taken.saturating_add(len);
        if self.taken >= half {
            let inc = u32::try_from(self.taken).unwrap_or(WINDOW).min(WINDOW);
            frame_into(&mut out, kind::WINDOW_UPDATE, 0, 0, &inc.to_be_bytes());
            self.taken = 0;
        }
        if stream {
            p.taken = p.taken.saturating_add(len);
            if p.taken >= half {
                let inc = u32::try_from(p.taken).unwrap_or(WINDOW).min(WINDOW);
                frame_into(&mut out, kind::WINDOW_UPDATE, 0, p.id, &inc.to_be_bytes());
                p.taken = 0;
            }
        }
        if out.is_empty() {
            return Ok(());
        }
        self.send(&out)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.w.write_all(bytes).map_err(io)?;
        self.w.flush().map_err(io)
    }

    /// A header block: `f`'s fragment and its CONTINUATIONs (§6.10), decoded.
    fn header_block(&mut self, f: Frame) -> Result<Vec<Field>, Error> {
        let mut fragment = unpad(&f)?;
        if f.flags & flag::PRIORITY != 0 {
            fragment = fragment
                .get(5..)
                .ok_or_else(|| Error("a priority past its frame".into()))?;
        }
        let mut block = fragment.to_vec();
        let mut ended = f.flags & flag::END_HEADERS != 0;
        while !ended {
            let c = self.read_frame()?;
            if c.kind != kind::CONTINUATION || c.stream != f.stream {
                return bad("a header block not continued");
            }
            if block.len().saturating_add(c.payload.len()) > MAX_HEADER_LIST {
                return bad("a header block larger than allowed");
            }
            block.extend_from_slice(&c.payload);
            ended = c.flags & flag::END_HEADERS != 0;
        }
        self.decoder.decode(&block).map_err(|e| Error(e.to_string()))
    }

    fn read_frame(&mut self) -> Result<Frame, Error> {
        let mut head = [0u8; 9];
        self.r.read_exact(&mut head).map_err(io)?;
        let [l0, l1, l2, kind, flags, s0, s1, s2, s3] = head;
        let len = (usize::from(l0) << 16) | (usize::from(l1) << 8) | usize::from(l2);
        if len > MAX_FRAME {
            return bad(format!("a frame of {len} bytes, past {MAX_FRAME}"));
        }
        let mut payload = vec![0u8; len];
        self.r.read_exact(&mut payload).map_err(io)?;
        Ok(Frame {
            kind,
            flags,
            stream: u32::from_be_bytes([s0, s1, s2, s3]) & 0x7fff_ffff,
            payload,
        })
    }
}

/// A frame's payload less its padding (§6.1, §6.2).
fn unpad(f: &Frame) -> Result<&[u8], Error> {
    if f.flags & flag::PADDED == 0 {
        return Ok(&f.payload);
    }
    let Some((&pad, rest)) = f.payload.split_first() else {
        return bad("a padded frame without its pad length");
    };
    rest.len()
        .checked_sub(usize::from(pad))
        .and_then(|n| rest.get(..n))
        .ok_or_else(|| Error("padding past its frame".into()))
}

fn frame_into(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
    let [_, l0, l1, l2] = u32::try_from(payload.len()).unwrap_or(0).to_be_bytes();
    out.extend_from_slice(&[l0, l1, l2, kind, flags]);
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
}

/// An error code's name (§7).
fn error_code(code: u32) -> String {
    let name = match code {
        0x0 => "NO_ERROR",
        0x1 => "PROTOCOL_ERROR",
        0x2 => "INTERNAL_ERROR",
        0x3 => "FLOW_CONTROL_ERROR",
        0x4 => "SETTINGS_TIMEOUT",
        0x5 => "STREAM_CLOSED",
        0x6 => "FRAME_SIZE_ERROR",
        0x7 => "REFUSED_STREAM",
        0x8 => "CANCEL",
        0x9 => "COMPRESSION_ERROR",
        0xa => "CONNECT_ERROR",
        0xb => "ENHANCE_YOUR_CALM",
        0xc => "INADEQUATE_SECURITY",
        0xd => "HTTP_1_1_REQUIRED",
        _ => return format!("error code {code:#x}"),
    };
    name.to_string()
}
