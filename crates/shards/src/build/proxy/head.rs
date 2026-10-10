//! A request as Go 1.26's net/http reads one for BuildKit's proxy (D110): its head as
//! `http.ReadRequest` reads it (the request line, `ParseHTTPVersion`, textproto's MIME
//! header with its continuation lines and canonical keys), the server's own checks after
//! (`conn.readRequest`: the version, `Host`, field names and values), its body's framing
//! (`readTransfer`), and what the server answers a request it cannot read. A head is
//! bounded as the server bounds it: 1 MiB and 4 KiB (`initialReadLimitSize`).

use std::io::{self, BufRead, Read};

/// The most a request's head may take (`DefaultMaxHeaderBytes` + 4096).
pub const MOST: usize = (1 << 20) + 4096;

/// The most of a request's body Go's server reads and drops to keep a connection after a
/// handler that read none of it (`maxPostHandlerReadBytes`).
pub const DISCARD: u64 = 256 << 10;

/// A request's head: its method, target (`RequestURI`), version, and fields, keys as Go
/// canonicalizes them, values without the spaces around them, in the order they came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub major: u8,
    pub minor: u8,
    pub fields: Vec<(String, String)>,
}

/// Why a request could not be read, and what Go's server answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// The connection ended or its time ran out: no answer (`isCommonNetReadError`).
    Gone,
    /// A head past [`MOST`].
    TooLarge,
    /// A transfer coding but chunked.
    Coding,
    /// One of the server's checks, its status and words (`statusError`).
    Status(u16, &'static str),
    /// Anything else malformed.
    Malformed,
}

const ERROR_HEADERS: &str = "\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n";

impl Fault {
    /// What Go's server writes before it closes the connection (`conn.serve`).
    pub fn answer(&self) -> Option<Vec<u8>> {
        let text = match self {
            Fault::Gone => return None,
            Fault::TooLarge => {
                let e = "431 Request Header Fields Too Large";
                format!("HTTP/1.1 {e}{ERROR_HEADERS}{e}")
            }
            Fault::Coding => {
                format!("HTTP/1.1 501 Not Implemented{ERROR_HEADERS}Unsupported transfer encoding")
            }
            Fault::Status(code, why) => {
                let s = format!("{code} {}: {why}", status_text(*code));
                format!("HTTP/1.1 {s}{ERROR_HEADERS}{s}")
            }
            Fault::Malformed => {
                let e = "400 Bad Request";
                format!("HTTP/1.1 {e}{ERROR_HEADERS}{e}")
            }
        };
        Some(text.into_bytes())
    }
}

/// `http.StatusText`.
pub fn status_text(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

/// A token's byte (RFC 9110 §5.6.2), as `httpguts.IsTokenRune` takes one.
fn token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

/// textproto's `validHeaderValueByte`: VCHAR, SP, HTAB and obs-text.
fn value_byte(c: u8) -> bool {
    c >= 0x80 || (0x20..0x7f).contains(&c) || c == b'\t'
}

/// httpguts' `validHostByte`.
fn host_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!$%&()*+,-.:;=[']_~".contains(&c)
}

/// textproto's `canonicalMIMEHeaderKey`: the key with its first letter and each after a
/// `-` upper-cased, the rest lower, where it is a token; one with a space kept as it is.
fn canonical(key: &[u8]) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let mut spaced = false;
    for &c in key {
        if token(c) {
            continue;
        }
        if c == b' ' {
            spaced = true;
            continue;
        }
        return None;
    }
    if spaced {
        return Some(String::from_utf8_lossy(key).into_owned());
    }
    let mut upper = true;
    let out: Vec<u8> = key
        .iter()
        .map(|&c| {
            let c = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == b'-';
            c
        })
        .collect();
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// textproto's `trim`: spaces and tabs off both ends.
fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&c| c != b' ' && c != b'\t')
        .map_or(start, |i| i + 1);
    s.get(start..end).unwrap_or_default()
}

/// A reader of one head, counting what it takes against [`MOST`].
struct Lines<'r, R: BufRead> {
    r: &'r mut R,
    left: usize,
}

impl<R: BufRead> Lines<'_, R> {
    /// One line, its `\n` and a `\r` before it taken off (bufio's `ReadLine`). An end
    /// before it is [`Fault::Gone`]; past the head's bound, [`Fault::TooLarge`].
    fn line(&mut self) -> Result<Vec<u8>, Fault> {
        let mut line = Vec::new();
        loop {
            let buf = match self.r.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Fault::Gone),
            };
            if buf.is_empty() {
                // A head cut short ends as Go's reader ends it: unanswered.
                return Err(Fault::Gone);
            }
            let (take, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            if take > self.left {
                return Err(Fault::TooLarge);
            }
            self.left -= take;
            line.extend_from_slice(buf.get(..take).unwrap_or_default());
            self.r.consume(take);
            if done {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
        }
    }

    /// The next byte, unread.
    fn peek(&mut self) -> Option<u8> {
        loop {
            match self.r.fill_buf() {
                Ok(b) => return b.first().copied(),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
    }

    /// textproto's `skipSpace`: how many spaces and tabs it passed over.
    fn skip_space(&mut self) -> usize {
        let mut n = 0;
        while matches!(self.peek(), Some(b' ' | b'\t')) && self.left > 0 {
            self.r.consume(1);
            self.left -= 1;
            n += 1;
        }
        n
    }
}

/// `ParseHTTPVersion`.
fn version(v: &[u8]) -> Option<(u8, u8)> {
    match v {
        b"HTTP/1.1" => Some((1, 1)),
        b"HTTP/1.0" => Some((1, 0)),
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() =>
        {
            Some((major - b'0', minor - b'0'))
        }
        _ => None,
    }
}

/// Reads a request's head from `r`, as Go's server reads one where `server`, else as
/// `http.ReadRequest` alone reads one (a request in a CONNECT tunnel, which BuildKit reads
/// so). `after_post` passes over blank lines first, as the server does after a POST.
pub fn read(r: &mut impl BufRead, server: bool, after_post: bool) -> Result<Head, Fault> {
    let mut lines = Lines { r, left: MOST };
    if after_post {
        while matches!(lines.peek(), Some(b'\r' | b'\n')) && lines.left > 0 {
            lines.r.consume(1);
            lines.left -= 1;
        }
    }
    let line = lines.line()?;
    // parseRequestLine: METHOD SP TARGET SP VERSION, split at the first two spaces.
    let mut parts = line.splitn(3, |&b| b == b' ');
    let (Some(method), Some(target), Some(proto)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(Fault::Malformed);
    };
    if method.is_empty() || !method.iter().all(|&c| token(c)) {
        return Err(Fault::Malformed);
    }
    let (major, minor) = version(proto).ok_or(Fault::Malformed)?;
    let target = String::from_utf8(target.to_vec()).map_err(|_| Fault::Malformed)?;
    let method = String::from_utf8(method.to_vec()).map_err(|_| Fault::Malformed)?;
    // The target as `ParseRequestURI` reads it: CONNECT's an authority, unless a path.
    let raw = if method == "CONNECT" && !target.starts_with('/') {
        format!("http://{target}")
    } else {
        target.clone()
    };
    shards_dockerfile::url::parse_request_uri(raw.as_bytes()).map_err(|_| Fault::Malformed)?;
    // readMIMEHeader: no leading space on its first line.
    if matches!(lines.peek(), Some(b' ' | b'\t')) {
        return Err(Fault::Malformed);
    }
    let mut fields = Vec::new();
    loop {
        let first = lines.line()?;
        if first.is_empty() {
            break;
        }
        if !first.contains(&b':') {
            return Err(Fault::Malformed);
        }
        let mut kv = trim(&first).to_vec();
        // Continuation lines, each joined by one space.
        while lines.skip_space() > 0 {
            kv.push(b' ');
            let next = lines.line()?;
            kv.extend_from_slice(trim(&next));
        }
        let colon = kv.iter().position(|&b| b == b':').ok_or(Fault::Malformed)?;
        let (k, v) = kv.split_at(colon);
        let v = v.get(1..).unwrap_or_default();
        let key = canonical(k).ok_or(Fault::Malformed)?;
        if !v.iter().all(|&c| value_byte(c)) {
            return Err(Fault::Malformed);
        }
        let start = v.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(v.len());
        // A value that is not UTF-8 (obs-text) cannot be passed on as shards' client
        // writes fields: refused here (D110's recorded difference).
        let value =
            String::from_utf8(v.get(start..).unwrap_or_default().to_vec()).map_err(|_| Fault::Malformed)?;
        fields.push((key, value));
    }
    let hosts = fields.iter().filter(|(k, _)| k == "Host").count();
    if hosts > 1 {
        return Err(Fault::Malformed);
    }
    let head = Head {
        method,
        target,
        major,
        minor,
        fields,
    };
    // readRequest's transfer before the server's checks, as Go reads them.
    head.framing()?;
    if server {
        // A request of another major version is refused, HTTP/2's preface among them.
        if major != 1 {
            return Err(Fault::Status(505, "unsupported protocol version"));
        }
        if head.at_least(1, 1) && hosts == 0 && head.method != "CONNECT" {
            return Err(Fault::Status(400, "missing required Host header"));
        }
        if let Some(h) = head.get("Host")
            && !h.bytes().all(host_byte)
        {
            return Err(Fault::Status(400, "malformed Host header"));
        }
        if head.fields.iter().any(|(k, _)| !k.bytes().all(token)) {
            return Err(Fault::Status(400, "invalid header name"));
        }
    }
    Ok(head)
}

/// How a request's body is framed (`readTransfer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Length(u64),
    Chunked,
}

impl Head {
    pub fn at_least(&self, major: u8, minor: u8) -> bool {
        (self.major, self.minor) >= (major, minor)
    }

    /// The first value of field `key` (any case).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn values<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// Whether field `key` lists `token` (`hasToken`).
    pub fn has_token(&self, key: &str, token: &str) -> bool {
        self.values(key)
            .flat_map(|v| v.split(','))
            .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(token))
    }

    /// `Request.Close` (`shouldClose`): HTTP/1.0 unless it asks to be kept alive, or one
    /// that says `Connection: close`.
    pub fn wants_close(&self) -> bool {
        let close = self.has_token("Connection", "close");
        if (self.major, self.minor) == (1, 0) {
            return close || !self.has_token("Connection", "keep-alive");
        }
        close
    }

    /// Its body's framing, as `readTransfer` reads a request's: chunked alone where it has
    /// a transfer coding (on HTTP/1.1; HTTP/1.0's ignored), else its length, else none.
    pub fn framing(&self) -> Result<Framing, Fault> {
        let codings: Vec<&str> = self.values("Transfer-Encoding").collect();
        if !codings.is_empty() && self.at_least(1, 1) {
            return match codings.as_slice() {
                [only] if only.eq_ignore_ascii_case("chunked") => Ok(Framing::Chunked),
                _ => Err(Fault::Coding),
            };
        }
        let lengths: Vec<&str> = self.values("Content-Length").collect();
        let Some(first) = lengths.first().map(|l| l.trim_matches([' ', '\t'])) else {
            return Ok(Framing::Length(0));
        };
        if lengths.iter().any(|l| l.trim_matches([' ', '\t']) != first) {
            return Err(Fault::Malformed);
        }
        // strconv.ParseUint(s, 10, 63).
        let valid = !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit());
        match first.parse::<u64>() {
            Ok(n) if valid && n < 1 << 63 => Ok(Framing::Length(n)),
            _ => Err(Fault::Malformed),
        }
    }

    /// The fields a request passes on upstream as BuildKit's handler sends it on:
    /// without the hop-by-hop ones `stripProxyHeaders` takes off, `Accept-Encoding`, or
    /// what the client writes itself (`Host`, `Content-Length`); `Cache-Control: no-cache`
    /// added for `Pragma: no-cache` alone, as Go's `fixPragmaCacheControl` adds it.
    pub fn forwarded(&self) -> Vec<(&str, &str)> {
        const DROPPED: [&str; 12] = [
            "Connection",
            "Keep-Alive",
            "Proxy-Authenticate",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Te",
            "Trailer",
            "Transfer-Encoding",
            "Upgrade",
            "Accept-Encoding",
            "Host",
            "Content-Length",
        ];
        let mut out: Vec<(&str, &str)> = self
            .fields
            .iter()
            .filter(|(k, _)| !DROPPED.iter().any(|d| k.eq_ignore_ascii_case(d)))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if self.values("Pragma").next() == Some("no-cache") && self.get("Cache-Control").is_none() {
            out.push(("Cache-Control", "no-cache"));
        }
        out
    }
}

/// A request's body, read as its framing says (Go's `body`, chunked as `internal/chunked`
/// reads it: CRLF lines of at most 4 KiB, sizes of at most 16 hex digits, extensions and
/// trailers passed over).
pub struct Body<'r, R: BufRead> {
    r: &'r mut R,
    framing: Framing,
    left: u64,
    done: bool,
}

impl<'r, R: BufRead> Body<'r, R> {
    pub fn new(r: &'r mut R, framing: Framing) -> Body<'r, R> {
        let left = match framing {
            Framing::Length(n) => n,
            Framing::Chunked => 0,
        };
        Body {
            r,
            framing,
            left,
            done: framing == Framing::Length(0),
        }
    }

    /// Whether it has been read to its end.
    pub fn ended(&self) -> bool {
        self.done
    }

    fn line(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let buf = self.r.fill_buf()?;
            if buf.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "a chunk line cut short",
                ));
            }
            let (take, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            if line.len() + take > 4096 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "header line too long"));
            }
            line.extend_from_slice(buf.get(..take).unwrap_or_default());
            self.r.consume(take);
            if done {
                return Ok(line);
            }
        }
    }
}

impl<R: BufRead> Read for Body<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        loop {
            if self.done || buf.is_empty() {
                return Ok(0);
            }
            if self.left == 0 {
                // Only a chunked body comes here with nothing left of a chunk.
                let line = self.line()?;
                let line = line
                    .strip_suffix(b"\r\n")
                    .filter(|l| !l.contains(&b'\r'))
                    .ok_or_else(|| bad("malformed chunked encoding"))?;
                let size = trim(line.split(|&b| b == b';').next().unwrap_or_default());
                if size.is_empty() || size.len() > 16 {
                    return Err(bad("invalid byte in chunk length"));
                }
                let n = size.iter().try_fold(0u64, |n, &d| {
                    char::from(d).to_digit(16).map(|v| n << 4 | u64::from(v))
                });
                self.left = n.ok_or_else(|| bad("invalid byte in chunk length"))?;
                if self.left == 0 {
                    // The trailer, passed over, then the end.
                    loop {
                        let t = self.line()?;
                        if t == b"\r\n" || t == b"\n" {
                            break;
                        }
                    }
                    self.done = true;
                    return Ok(0);
                }
                continue;
            }
            let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
            let n = self.r.read(buf.get_mut(..want).unwrap_or_default())?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF"));
            }
            self.left -= n as u64;
            if self.left == 0 {
                match self.framing {
                    Framing::Length(_) => self.done = true,
                    Framing::Chunked => {
                        let mut crlf = [0u8; 2];
                        self.r.read_exact(&mut crlf)?;
                        if &crlf != b"\r\n" {
                            return Err(bad("malformed chunked encoding"));
                        }
                    }
                }
            }
            return Ok(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(raw: &str) -> Result<Head, Fault> {
        read(&mut io::BufReader::new(raw.as_bytes()), true, false)
    }

    #[test]
    fn heads_are_read_as_gos_server_reads_them() {
        let h =
            head("GET http://172.18.0.2/hello HTTP/1.1\r\nhost:  h \r\nX-a: 1\r\n\tmore\r\nx-a:2\r\n\r\n")
                .unwrap();
        assert_eq!(
            (h.method.as_str(), h.target.as_str(), h.major, h.minor),
            ("GET", "http://172.18.0.2/hello", 1, 1)
        );
        assert_eq!(
            h.fields,
            [
                ("Host".into(), "h".into()),
                ("X-A".into(), "1 more".into()),
                ("X-A".into(), "2".into())
            ]
        );
        let c = head("CONNECT example.com:443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(c.target, "example.com:443");
        // HTTP/1.0 needs no Host.
        assert!(head("GET / HTTP/1.0\r\n\r\n").unwrap().wants_close());
        for (raw, fault) in [
            (
                "GET / HTTP/1.1\r\n\r\n",
                Fault::Status(400, "missing required Host header"),
            ),
            (
                "GET / HTTP/2.0\r\nHost: h\r\n\r\n",
                Fault::Status(505, "unsupported protocol version"),
            ),
            (
                "GET / HTTP/1.1\r\nHost: a b\r\n\r\n",
                Fault::Status(400, "malformed Host header"),
            ),
            (
                "GET / HTTP/1.1\r\nHost: h\r\nKey : v\r\n\r\n",
                Fault::Status(400, "invalid header name"),
            ),
            ("GET / HTTP/1.1\r\nHost: h\r\nHost: h\r\n\r\n", Fault::Malformed),
            ("GET /\r\n\r\n", Fault::Malformed),
            ("G(T / HTTP/1.1\r\n\r\n", Fault::Malformed),
            ("GET / HTTP/11\r\n\r\n", Fault::Malformed),
            ("GET x HTTP/1.1\r\nHost: h\r\n\r\n", Fault::Malformed),
            ("GET / HTTP/1.1\r\n Host: h\r\n\r\n", Fault::Malformed),
            ("GET / HTTP/1.1\r\nHost h\r\n\r\n", Fault::Malformed),
            ("GET / HTTP/1.1\r\nHost: h\r\nX: a\x01\r\n\r\n", Fault::Malformed),
            (
                "GET / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: gzip\r\n\r\n",
                Fault::Coding,
            ),
            (
                "GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
                Fault::Malformed,
            ),
            (
                "GET / HTTP/1.1\r\nHost: h\r\nContent-Length: -1\r\n\r\n",
                Fault::Malformed,
            ),
            ("GET / HTTP/1.1\r\nHost: h\r\n", Fault::Gone),
        ] {
            assert_eq!(head(raw), Err(fault), "{raw:?}");
        }
        let huge = format!("GET / HTTP/1.1\r\nHost: h\r\nX: {}\r\n\r\n", "a".repeat(MOST));
        assert_eq!(head(&huge), Err(Fault::TooLarge));
        // In a tunnel, `http.ReadRequest` alone: no Host needed.
        assert!(
            read(
                &mut io::BufReader::new(&b"GET / HTTP/1.1\r\n\r\n"[..]),
                false,
                false
            )
            .is_ok()
        );
        // After a POST, blank lines before the next request are passed over.
        assert!(
            read(
                &mut io::BufReader::new(&b"\r\n\r\nGET / HTTP/1.0\r\n\r\n"[..]),
                true,
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn faults_are_answered_as_gos_server_answers_them() {
        let said = |f: Fault| String::from_utf8(f.answer().unwrap()).unwrap();
        assert_eq!(
            said(Fault::Status(400, "missing required Host header")),
            "HTTP/1.1 400 Bad Request: missing required Host header\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request: missing required Host header"
        );
        assert_eq!(
            said(Fault::Malformed),
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request"
        );
        assert_eq!(
            said(Fault::TooLarge),
            "HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n431 Request Header Fields Too Large"
        );
        assert_eq!(
            said(Fault::Coding),
            "HTTP/1.1 501 Not Implemented\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nUnsupported transfer encoding"
        );
        assert!(Fault::Gone.answer().is_none());
    }

    #[test]
    fn bodies_are_read_as_their_framing_says() {
        let mut chunked = io::BufReader::new(&b"3;ext\r\nabc\r\n2\r\nde\r\n0\r\nT: v\r\n\r\nNEXT"[..]);
        let mut body = Body::new(&mut chunked, Framing::Chunked);
        let mut got = Vec::new();
        body.read_to_end(&mut got).unwrap();
        assert!(body.ended());
        assert_eq!(got, b"abcde");
        let mut rest = Vec::new();
        chunked.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"NEXT");
        let mut sized = io::BufReader::new(&b"hello world"[..]);
        let mut got = Vec::new();
        Body::new(&mut sized, Framing::Length(5))
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"hello");
        for bad in [
            &b"x\r\n"[..],
            b"3\r\nabcX\r\n",
            b"3\nabc\r\n0\r\n\r\n",
            b"11111111111111111\r\n",
        ] {
            let mut r = io::BufReader::new(bad);
            assert!(
                Body::new(&mut r, Framing::Chunked)
                    .read_to_end(&mut Vec::new())
                    .is_err(),
                "{bad:?}"
            );
        }
        let mut short = io::BufReader::new(&b"abc"[..]);
        assert!(
            Body::new(&mut short, Framing::Length(5))
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }

    #[test]
    fn requests_pass_on_what_buildkit_passes_on() {
        let h = head(
            "GET / HTTP/1.1\r\nHost: h\r\nConnection: x\r\nProxy-Authorization: p\r\nAccept-Encoding: gzip\r\nUser-Agent: curl\r\nPragma: no-cache\r\nTE: trailers\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            h.forwarded(),
            [
                ("User-Agent", "curl"),
                ("Pragma", "no-cache"),
                ("Cache-Control", "no-cache")
            ]
        );
    }
}
