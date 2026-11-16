//! An HTTP/2 client (RFC 9113) of one connection over a byte pipe, with prior knowledge
//! (§3.3), as grpc-go dials a frontend's stdio. Its streams open one at a time, each a
//! request and its response; SETTINGS, PING, WINDOW_UPDATE and GOAWAY are answered as
//! they come. Its frames, settings, flow control both ways (§5.2, §6.9) and field
//! compression are shards-http2's, the credential injector's own (D242, R12): every frame
//! is bounded by this side's SETTINGS_MAX_FRAME_SIZE (the protocol's, never raised), every
//! header list by [`MAX_HEADER_LIST`], every body by what the caller allows.

use std::io::{Read, Write};

use shards_http2::hpack::{Decoded, Field, Indexing};
use shards_http2::{self as http2, Block, End, Head, Holds, Hop, Settings};

/// The window this side gives each stream and the connection: a gRPC message's most,
/// 16 MiB (grpcclient's MaxCallRecvMsgSize), with its prefix, fits one stream's.
pub const WINDOW: u32 = 1 << 25;
/// The header lists this side takes: BuildKit's errors carry their stacks in
/// `grpc-status-details-bin`.
const MAX_HEADER_LIST: usize = 1 << 20;

/// What a connection or a stream ended with instead of its response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http2: {}", self.0)
    }
}

impl From<http2::Error> for Error {
    fn from(e: http2::Error) -> Error {
        Error(e.why.to_string())
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
}

/// One client connection.
#[derive(Debug)]
pub struct Conn<R, W> {
    r: R,
    w: W,
    /// This side's end of the connection: the peer's settings, each way's windows and
    /// field compression.
    hop: Hop,
    next_stream: u32,
    /// The last stream the peer will answer, once it has said GOAWAY.
    last_stream: Option<u32>,
}

impl<R: Read, W: Write> Conn<R, W> {
    /// A connection over `r` and `w`: the preface, this side's settings (no push, its
    /// streams' windows), and the connection's own window raised to [`WINDOW`].
    pub fn new(r: R, mut w: W) -> Result<Conn<R, W>, Error> {
        let hop = Hop::new(
            Holds {
                stream: WINDOW,
                connection: WINDOW,
            },
            Settings::INITIAL.header_table_size as usize,
        );
        let mut out = Vec::new();
        hop.preface(End::Client, &mut out);
        w.write_all(&out).map_err(io)?;
        w.flush().map_err(io)?;
        Ok(Conn {
            r,
            w,
            hop,
            next_stream: 1,
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
            .filter(|s| *s <= http2::MOST_STREAM)
            .ok_or_else(|| Error("no stream identifiers left".into()))?;
        let mut p = Pending {
            id,
            phase: Phase::Headers,
            response: Response::default(),
            most,
        };
        let exchanged = self.exchange(&mut p, headers, body);
        // The stream's windows and credit let go of, however it ended.
        self.hop.close(id);
        exchanged.map(|()| p.response)
    }

    /// `headers`, then `body` as the peer's windows let it go, on `p`'s stream, and its
    /// response read to its end.
    fn exchange(&mut self, p: &mut Pending, headers: &[(&str, &str)], body: &[u8]) -> Result<(), Error> {
        // Literals without indexing (RFC 7541 §6.2.2), which change no table.
        let fields: Vec<Field> = headers
            .iter()
            .map(|(name, value)| Field::new(name.as_bytes(), value.as_bytes(), Indexing::Without))
            .collect();
        let mut block = Vec::new();
        self.hop.encoder.encode(&fields, &mut block);
        let mut out = Vec::new();
        http2::headers(
            p.id,
            &block,
            body.is_empty(),
            self.hop.peer.max_frame_size,
            &mut out,
        );
        self.w.write_all(&out).map_err(io)?;
        let mut sent = 0;
        while let Some(rest) = body.get(sent..).filter(|rest| !rest.is_empty()) {
            out.clear();
            sent += self.hop.send_data(p.id, rest, true, &mut out);
            self.w.write_all(&out).map_err(io)?;
            if sent < body.len() {
                // The windows spent: what the peer says next may give them room.
                self.w.flush().map_err(io)?;
                self.next(p)?;
                if p.phase == Phase::Done {
                    return Ok(());
                }
            }
        }
        self.w.flush().map_err(io)?;
        while p.phase != Phase::Done {
            self.next(p)?;
        }
        Ok(())
    }

    /// The next frame, applied: the connection's own, or the stream's, or another's,
    /// whose header block still changes the table (§4.3) and whose data still takes the
    /// connection's window.
    fn next(&mut self, p: &mut Pending) -> Result<(), Error> {
        let (head, payload) = self.read_frame()?;
        let mut out = Vec::new();
        match head.kind {
            http2::SETTINGS => self.hop.settings(head, &payload, &mut out)?,
            http2::PING => http2::ping(head, &payload, &mut out)?,
            http2::WINDOW_UPDATE if head.stream == 0 || head.stream == p.id => {
                self.hop.window_update(head, &payload)?;
            }
            http2::GOAWAY => {
                let last = http2::word(&payload, 0)
                    .map(|last| last & http2::MOST_STREAM)
                    .ok_or_else(|| Error("a GOAWAY shorter than its fields".into()))?;
                self.last_stream = Some(last);
                if p.id > last {
                    return bad("the peer said GOAWAY before this stream");
                }
            }
            http2::PUSH_PROMISE => return bad("a PUSH_PROMISE, which this side did not allow"),
            http2::CONTINUATION => return bad("a CONTINUATION after a complete header block"),
            http2::HEADERS => self.headers(p, head, &payload)?,
            http2::DATA => self.data(p, head, &payload, &mut out)?,
            http2::RST_STREAM if head.stream == 0 => return bad("a RST_STREAM on the connection"),
            http2::RST_STREAM if head.stream == p.id => {
                let code = http2::word(&payload, 0).unwrap_or(0);
                return bad(format!("the stream was reset ({})", error_code(code)));
            }
            // PRIORITY, another stream's WINDOW_UPDATE or RST_STREAM, and types this side
            // does not know are passed over (§4.1, §5.5, §6.9).
            _ => {}
        }
        if out.is_empty() {
            return Ok(());
        }
        self.send(&out)
    }

    /// A header block: `head`'s fragment and its CONTINUATIONs (§6.10), decoded whole; on
    /// the stream, its response's headers or trailers.
    fn headers(&mut self, p: &mut Pending, head: Head, payload: &[u8]) -> Result<(), Error> {
        let (mut block, mut whole) = Block::headers(head, payload)?;
        while !whole {
            let (next, payload) = self.read_frame()?;
            whole = block.continued(next, &payload, MAX_HEADER_LIST)?;
        }
        let fields = match block.decode(&mut self.hop.decoder, MAX_HEADER_LIST)? {
            Decoded::Fields(fields) => fields,
            Decoded::TooLarge => return bad("a header list larger than allowed"),
        };
        if head.stream != p.id {
            return Ok(());
        }
        let ends = head.flags & http2::END_STREAM != 0;
        match p.phase {
            Phase::Headers => p.response.headers = fields,
            Phase::Body if ends => p.response.trailers = fields,
            _ => return bad("headers after the trailers"),
        }
        p.phase = if ends { Phase::Done } else { Phase::Body };
        Ok(())
    }

    /// A DATA frame, its octets given back to the peer's windows as they are taken (§6.9):
    /// on the stream, its response's body, the stream's own window while it goes on; any
    /// other stream's, closed here, the connection's alone.
    fn data(&mut self, p: &mut Pending, head: Head, payload: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        if head.stream == 0 {
            return bad("DATA on the connection");
        }
        self.hop.received(head)?;
        let data = http2::data(head, payload)?;
        let taken = head.length as usize;
        if head.stream != p.id {
            self.hop.released_on_connection(taken, out);
            self.hop.close(head.stream);
            return Ok(());
        }
        if p.phase != Phase::Body {
            return bad("data before the response's headers");
        }
        if p.response.body.len().saturating_add(data.len()) > p.most {
            return bad(format!("a response larger than {} bytes", p.most));
        }
        p.response.body.extend_from_slice(data);
        if head.flags & http2::END_STREAM == 0 {
            self.hop.released(head.stream, taken, out);
        } else {
            self.hop.released_on_connection(taken, out);
            p.phase = Phase::Done;
        }
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.w.write_all(bytes).map_err(io)?;
        self.w.flush().map_err(io)
    }

    fn read_frame(&mut self) -> Result<(Head, Vec<u8>), Error> {
        let mut bytes = [0u8; http2::HEAD];
        self.r.read_exact(&mut bytes).map_err(io)?;
        let head = Head::read(&bytes, Settings::INITIAL.max_frame_size)?
            .ok_or_else(|| Error("a frame's header cut short".into()))?;
        let mut payload = vec![0u8; head.length as usize];
        self.r.read_exact(&mut payload).map_err(io)?;
        Ok((head, payload))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use shards_http2::hpack::Encoder;
    use shards_http2::{ACK, END_HEADERS, END_STREAM};

    fn put(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes()[1..]);
        out.extend_from_slice(&[kind, flags]);
        out.extend_from_slice(&stream.to_be_bytes());
        out.extend_from_slice(payload);
    }

    /// Each frame's type, flags, stream and payload, after the client's preface.
    fn frames(wire: &[u8]) -> Vec<(u8, u8, u32, Vec<u8>)> {
        let mut wire = wire.strip_prefix(http2::PREFACE).unwrap();
        let mut all = Vec::new();
        while let Some((h, payload)) = http2::frame(wire, u32::MAX).unwrap() {
            all.push((h.kind, h.flags, h.stream, payload.to_vec()));
            wire = &wire[http2::HEAD + payload.len()..];
        }
        assert!(wire.is_empty(), "whole frames");
        all
    }

    /// Do: a request whose body is past the peer's first windows, the peer granting more
    /// after its SETTINGS, and the response's last DATA a whole frame. Expect the body held
    /// at the initial 65,535 octets until the peer grants more, then sent whole, its
    /// END_STREAM with its last octet; the response read whole; and the credit of its last
    /// frame given back to the connection alone, as the stream is closed by it (§5.1).
    #[test]
    fn a_body_waits_for_its_windows_and_a_closed_streams_credit_goes_to_the_connection() {
        let body = vec![7u8; 70_000];
        let mut server = Vec::new();
        put(&mut server, http2::SETTINGS, 0, 0, &[]);
        put(&mut server, http2::WINDOW_UPDATE, 0, 0, &10_000u32.to_be_bytes());
        put(&mut server, http2::WINDOW_UPDATE, 0, 1, &10_000u32.to_be_bytes());
        let mut status = Vec::new();
        Encoder::new(4096, 4096).encode(&[Field::new(b":status", b"200", Indexing::Without)], &mut status);
        put(&mut server, http2::HEADERS, END_HEADERS, 1, &status);
        put(&mut server, http2::DATA, 0, 1, &[1; 16_384]);
        put(&mut server, http2::DATA, END_STREAM, 1, &[2; 16_384]);
        let mut conn = Conn::new(std::io::Cursor::new(server), Vec::new()).unwrap();
        let headers = [
            (":method", "POST"),
            (":scheme", "http"),
            (":path", "/a.B/C"),
            (":authority", "localhost"),
        ];
        let response = conn.request(&headers, &body, 1 << 20).unwrap();
        let status: Vec<_> = response
            .headers
            .iter()
            .map(|f| (f.name.as_slice(), f.value.as_slice()))
            .collect();
        assert_eq!(status, [(&b":status"[..], &b"200"[..])]);
        assert_eq!(response.body.len(), 32_768);
        assert!(response.trailers.is_empty());
        let sent = frames(&conn.w);
        let data: Vec<_> = sent.iter().filter(|f| f.0 == http2::DATA).collect();
        assert_eq!(data.iter().map(|f| f.3.len()).sum::<usize>(), body.len());
        assert!(data.iter().all(|f| f.3.len() <= 16_384 && f.2 == 1));
        assert!(data.iter().rev().skip(1).all(|f| f.1 == 0));
        assert_eq!(data.last().unwrap().1, END_STREAM);
        let before_ack: usize = sent
            .iter()
            .take_while(|f| !(f.0 == http2::SETTINGS && f.1 & ACK != 0))
            .filter(|f| f.0 == http2::DATA)
            .map(|f| f.3.len())
            .sum();
        assert_eq!(
            before_ack, 65_535,
            "held at the initial windows until the peer spoke"
        );
        let credit: Vec<(u32, u32)> = sent
            .iter()
            .filter(|f| f.0 == http2::WINDOW_UPDATE)
            .map(|f| (f.2, http2::word(&f.3, 0).unwrap()))
            .collect();
        assert_eq!(
            credit,
            [(0, WINDOW - 65_535), (1, 16_384), (0, 16_384), (0, 16_384)],
            "the preface's, then the first DATA's to the stream and the connection, the last's to the connection alone"
        );
    }
}
