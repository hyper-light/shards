//! The engine's half of HTTP/1.1, as dockerd's Go server writes it: an answer whole, a
//! stream of output in chunks as it comes, a connection taken over for a stream of its
//! own (`101 UPGRADED`), and the frames of Docker's multiplexed stream.

use std::io::{self, Write as _};
use std::os::unix::net::UnixStream;

use serde_json::{Value, json};

use crate::build::proxy::head::{self, Head};

/// What every answer carries: the API's version (moby api/server/middleware), the
/// platform its containers are (`Ostype`, which clients read from `_ping`), and the
/// server.
fn common() -> String {
    format!(
        "Api-Version: {}\r\nOstype: linux\r\nServer: shards/{} (linux)\r\n",
        super::VERSION,
        env!("CARGO_PKG_VERSION")
    )
}

/// An answer whole: its status, its content's type, its content, and fields of its own.
pub(super) struct Answer {
    pub status: u16,
    pub kind: &'static str,
    pub body: Vec<u8>,
    pub fields: Vec<(&'static str, String)>,
}

impl Answer {
    /// `v` as Go's encoder writes a value: its JSON and a newline.
    pub(super) fn json(status: u16, v: &Value) -> Answer {
        let mut body = serde_json::to_vec(v).unwrap_or_default();
        body.push(b'\n');
        Answer {
            status,
            kind: "application/json",
            body,
            fields: Vec::new(),
        }
    }

    /// A JSON document already written (inspect_doc's).
    pub(super) fn document(status: u16, json: String) -> Answer {
        let mut body = json.into_bytes();
        body.push(b'\n');
        Answer {
            status,
            kind: "application/json",
            body,
            fields: Vec::new(),
        }
    }

    /// An error, as the API says one: `{"message": ...}`.
    pub(super) fn error(status: u16, message: impl Into<String>) -> Answer {
        Answer::json(status, &json!({ "message": message.into() }))
    }

    /// An answer with nothing to say (204, 304), or `text`.
    pub(super) fn text(status: u16, text: &str) -> Answer {
        Answer {
            status,
            kind: if text.is_empty() {
                ""
            } else {
                "text/plain; charset=utf-8"
            },
            body: text.as_bytes().to_vec(),
            fields: Vec::new(),
        }
    }

    /// Writes it on `conn` for request `head`; `keep` says whether the connection stays
    /// open after.
    pub(super) fn write(&self, mut conn: &UnixStream, head: &Head, keep: bool) -> io::Result<()> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\n{}",
            self.status,
            head::status_text(self.status),
            common()
        );
        for (k, v) in &self.fields {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if !self.kind.is_empty() {
            out.push_str(&format!("Content-Type: {}\r\n", self.kind));
        }
        // No content with 1xx, 204 or 304 (RFC 9110 §6.4.1).
        let bodiless = matches!(self.status, 100..=199 | 204 | 304);
        if !bodiless {
            out.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        if !keep {
            out.push_str("Connection: close\r\n");
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        if !bodiless && head.method != "HEAD" {
            bytes.extend_from_slice(&self.body);
        }
        conn.write_all(&bytes)
    }
}

/// Output as an answer's content, sent as it comes: in chunks on HTTP/1.1, until the
/// connection closes on HTTP/1.0.
pub(super) struct Stream<'c> {
    conn: &'c UnixStream,
    chunked: bool,
}

impl<'c> Stream<'c> {
    /// Its head, written at once: dockerd's are, before what the stream waits for.
    pub(super) fn start(
        conn: &'c UnixStream,
        head: &Head,
        status: u16,
        kind: &str,
    ) -> io::Result<Stream<'c>> {
        let chunked = head.at_least(1, 1);
        let framing = if chunked {
            "Transfer-Encoding: chunked"
        } else {
            "Connection: close"
        };
        let out = format!(
            "HTTP/1.1 {status} {}\r\n{}Content-Type: {kind}\r\n{framing}\r\n\r\n",
            head::status_text(status),
            common()
        );
        (&mut &*conn).write_all(out.as_bytes())?;
        Ok(Stream { conn, chunked })
    }

    pub(super) fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn;
        if !self.chunked {
            return conn.write_all(bytes);
        }
        let mut out = format!("{:x}\r\n", bytes.len()).into_bytes();
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
        conn.write_all(&out)
    }

    /// `v` as Go's encoder writes it, a line of the stream.
    pub(super) fn json(&mut self, v: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(v).unwrap_or_default();
        line.push(b'\n');
        self.send(&line)
    }

    /// Its end: the last chunk, after which the connection may serve another request.
    pub(super) fn end(self) -> io::Result<()> {
        if self.chunked {
            (&mut &*self.conn).write_all(b"0\r\n\r\n")?;
        }
        Ok(())
    }
}

/// The media types of a stream of a container's output: raw, a terminal's, or Docker's
/// multiplexed one (stdcopy), each write a frame of its stream.
pub(super) const RAW: &str = "application/vnd.docker.raw-stream";
pub(super) const MULTIPLEXED: &str = "application/vnd.docker.multiplexed-stream";

/// One frame of the multiplexed stream: its stream (0 stdin, 1 stdout, 2 stderr), three
/// zero bytes, its length big-endian, then its bytes.
pub(super) fn frame(stream: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + bytes.len());
    out.extend_from_slice(&[stream, 0, 0, 0]);
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Takes `conn` over for a stream of its own, as dockerd's hijack does: `101 UPGRADED`
/// where the client asked to upgrade, multiplexed where `multiplexed` from API 1.42; else
/// `200 OK` and a raw stream.
pub(super) fn hijack(mut conn: &UnixStream, head: &Head, version: &str, multiplexed: bool) -> io::Result<()> {
    let out = if head.get("Upgrade").is_some() {
        let kind = if multiplexed && super::at_least(version, "1.42") {
            MULTIPLEXED
        } else {
            RAW
        };
        format!(
            "HTTP/1.1 101 UPGRADED\r\nContent-Type: {kind}\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n{}\r\n",
            common()
        )
    } else {
        format!("HTTP/1.1 200 OK\r\nContent-Type: {RAW}\r\n{}\r\n", common())
    };
    conn.write_all(out.as_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn frames_say_their_stream_and_length() {
        assert_eq!(frame(2, b"hi"), [2, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
        assert_eq!(frame(1, b""), [1, 0, 0, 0, 0, 0, 0, 0]);
    }

    fn head(raw: &str) -> Head {
        head::read(&mut io::BufReader::new(raw.as_bytes()), true, false).unwrap()
    }

    #[test]
    fn answers_and_streams_are_written_as_gos_server_writes_them() {
        use std::io::Read as _;
        let (a, mut b) = UnixStream::pair().unwrap();
        let get = head("GET /version HTTP/1.1\r\nHost: api\r\n\r\n");
        Answer::json(200, &json!({"a": 1})).write(&a, &get, true).unwrap();
        Answer::text(204, "").write(&a, &get, false).unwrap();
        let mut s = Stream::start(&a, &get, 200, "application/json").unwrap();
        s.json(&json!({"StatusCode": 0})).unwrap();
        s.end().unwrap();
        drop(a);
        let mut got = String::new();
        b.read_to_string(&mut got).unwrap();
        let common = common();
        assert_eq!(
            got,
            format!(
                "HTTP/1.1 200 OK\r\n{common}Content-Type: application/json\r\nContent-Length: 8\r\n\r\n{{\"a\":1}}\n\
                 HTTP/1.1 204 No Content\r\n{common}Connection: close\r\n\r\n\
                 HTTP/1.1 200 OK\r\n{common}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
                 11\r\n{{\"StatusCode\":0}}\n\r\n0\r\n\r\n"
            )
        );
    }
}
