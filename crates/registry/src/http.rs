//! A blocking HTTP/1.1 client for registries (docs/research/registry-pull.md R4). It reads
//! responses as Go 1.27.1's net/http reads them for Docker and containerd:
//! - Heads: 1xx responses are skipped, and one response's heads together may take 10 MiB
//!   (`Transport.maxHeaderResponseSize`).
//! - Bodies are framed as `fixLength` frames them (RFC 9112 §6.3):
//!   - none after HEAD, 1xx, 204 and 304;
//!   - chunked under `Transfer-Encoding: chunked`, the only coding accepted;
//!   - else by `Content-Length`, whose copies must agree;
//!   - else until the connection closes.
//! - Chunked bodies as `internal/chunked.go` reads them: lines end in CRLF and hold at most
//!   4096 bytes, sizes have at most 16 hex digits, and non-data overhead is bounded.
//!
//! Connections follow containerd's transport (`core/remotes/docker/registry.go`):
//! - 30 s to connect, racing addresses 300 ms apart (RFC 8305);
//! - 10 s for the TLS handshake, 30 s for the response head;
//! - at most 10 idle connections, each kept for 30 s.
//!
//! A body that makes no progress for 30 s fails too, where Go would wait on its context.
//! That context's cancellation is a [`Cancel`]: every request of a client it cancels
//! fails, at once.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};

use crate::url::{Scheme, Url};
use crate::{Error, ErrorKind};

const CONNECT: Duration = Duration::from_secs(30);
const RACE: Duration = Duration::from_millis(300);
/// How often a dial waiting on its attempts looks for a cancel.
const DIAL_POLL: Duration = Duration::from_millis(100);
const HANDSHAKE: Duration = Duration::from_secs(10);
/// How long a request waits for its response's head.
pub const HEAD: Duration = Duration::from_secs(30);
const STALL: Duration = Duration::from_secs(30);
const IDLE: Duration = Duration::from_secs(30);
const MAX_IDLE: usize = 10;
const MAX_HEAD: usize = 10 << 20;
const MAX_CHUNK_LINE: usize = 4096;
const MAX_TRAILER: usize = 4096;
const MAX_REDIRECTS: usize = 10;
/// How much of a redirect's body Go reads so its connection can be reused
/// (`maxBodySlurpSize`).
const DRAIN: u64 = 2 << 10;

/// The TLS configuration to use for a URL's host.
pub type TlsFor = dyn Fn(&Url) -> Result<Arc<ClientConfig>, Error> + Send + Sync;

pub struct Client {
    tls: Box<TlsFor>,
    /// Idle connections, which responses borrowing the client hand back.
    pool: Pool,
    user_agent: String,
    cancel: Option<Cancel>,
}

/// Stops a client's requests from another thread, as cancelling Go's request context
/// does: once cancelled, a request fails before it is sent, and every connection in use
/// is shut down, so that a read or write waiting on one fails at once (audit A07).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<Cancelling>);

#[derive(Debug, Default)]
struct Cancelling {
    /// Set under `live`'s lock, so a connection is watched or sees it.
    cancelled: AtomicBool,
    /// The connections in use, by number, and the next number.
    live: Mutex<(u64, HashMap<u64, TcpStream>)>,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn cancel(&self) {
        let live = self.0.live.lock().unwrap_or_else(PoisonError::into_inner);
        self.0.cancelled.store(true, Ordering::SeqCst);
        for tcp in live.1.values() {
            end(tcp);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), Error> {
        if self.is_cancelled() {
            return Err(Error::of(ErrorKind::Cancelled, "cancelled"));
        }
        Ok(())
    }

    /// Watches `tcp` while the returned guard lives: a cancel shuts it down, as it does at
    /// once one already made. A connection that cannot be watched still fails within
    /// [`STALL`].
    fn watch(&self, tcp: &TcpStream) -> Watch {
        let mut live = self.0.live.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            end(tcp);
            return Watch(None);
        }
        let Ok(copy) = tcp.try_clone() else {
            return Watch(None);
        };
        let n = live.0;
        live.0 = n.wrapping_add(1);
        live.1.insert(n, copy);
        Watch(Some((self.clone(), n)))
    }
}

/// Ends `tcp`'s reads and writes, whichever thread waits in them. A shutdown wakes a read
/// another thread waits in on Linux and macOS, where Winsock's leaves it waiting (the
/// cancel test took the 30 s read timeout on Windows), so there the socket's I/O is
/// cancelled too: `CancelIoEx` cancels a handle's I/O whichever thread issued it, and a
/// base provider's socket handle is a file handle.
fn end(tcp: &TcpStream) {
    let _ = tcp.shutdown(Shutdown::Both);
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        // SAFETY: CancelIoEx on a socket handle `tcp` holds open, with no OVERLAPPED:
        // it only marks the I/O pending on it cancelled.
        unsafe {
            windows_sys::Win32::System::IO::CancelIoEx(
                tcp.as_raw_socket() as windows_sys::Win32::Foundation::HANDLE,
                std::ptr::null(),
            );
        }
    }
}

/// A connection a [`Cancel`] watches, until this drops.
#[derive(Debug)]
struct Watch(Option<(Cancel, u64)>);

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some((cancel, n)) = self.0.take() {
            cancel
                .0
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .1
                .remove(&n);
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

/// Where [`Client::follow`] may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// Wherever a redirect points; each hop's credentials come from `authorize`.
    Anywhere,
    /// Only within the first URL's origin: scheme, host and port.
    SameOrigin,
}

pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    /// Fields besides `Host`, `User-Agent` and `Content-Length`, which the client writes.
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    /// A body read from a file instead, so many bytes of it from its start: a blob
    /// uploaded. Read again from its start whenever the request is sent again.
    pub file: Option<(&'a std::fs::File, u64)>,
}

/// Shows no fields, body or query: they can carry credentials.
impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.url.target().split('?').next().unwrap_or_default();
        write!(f, "{} {}{}", self.method, self.url.authority(), path)
    }
}

/// A response. Reading it reads the body; once the body ends, the connection is reused.
#[derive(Debug)]
pub struct Response<'c> {
    pub status: u16,
    url: Url,
    headers: Vec<(String, String)>,
    body: Body<'c>,
}

impl Response<'_> {
    /// The URL that answered: after redirects, the last one.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The first value of the field `name`, which is matched without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every value of the field `name`, in order.
    pub fn headers<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Read for Response<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.body.read(buf)
    }
}

impl Client {
    pub fn new(tls: Box<TlsFor>, user_agent: &str) -> Client {
        Client {
            tls,
            pool: Pool::default(),
            user_agent: user_agent.to_string(),
            cancel: None,
        }
    }

    /// This client, its requests failing once `cancel` is cancelled.
    pub fn cancelled_by(mut self, cancel: Cancel) -> Client {
        self.cancel = Some(cancel);
        self
    }

    /// Sends `req` and reads the response head. A GET or HEAD that fails on a reused
    /// connection before any response arrived is sent once more on a new one, as Go
    /// retries replayable requests.
    pub fn send(&self, req: &Request<'_>) -> Result<Response<'_>, Error> {
        self.checked(self.send_now(req))
    }

    /// `result`, unless the client was cancelled: then that, whatever the failure it made.
    fn checked<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        match (&self.cancel, result) {
            (Some(cancel), Err(e)) if cancel.is_cancelled() => {
                Err(Error::of(ErrorKind::Cancelled, format!("cancelled: {e}")))
            }
            (_, result) => result,
        }
    }

    fn send_now(&self, req: &Request<'_>) -> Result<Response<'_>, Error> {
        if let Some(cancel) = &self.cancel {
            cancel.check()?;
        }
        let head = self.head(req)?;
        let key = Key::of(req.url);
        if let Some(mut conn) = self.pool.take(&key) {
            conn.watch = self.cancel.as_ref().map(|c| c.watch(conn.io.get_ref().tcp()));
            match self.exchange(conn, req, &head) {
                // The server may have closed it while it sat idle.
                Err(Failure::BeforeResponse(_)) if matches!(req.method, "GET" | "HEAD") => {}
                result => return result.map_err(Failure::into_error),
            }
        }
        let conn = self.connect(req.url, key)?;
        self.exchange(conn, req, &head).map_err(Failure::into_error)
    }

    /// Sends `req` and follows redirects as Go's client does (go1.27.1
    /// `net/http/client.go`): at most 10; 301, 302 and 303 turn a request other than GET or
    /// HEAD into a GET without a body; 307 and 308 keep the method and body. A redirect
    /// without a `Location` is the response.
    ///
    /// Each hop's `Authorization` comes from `authorize` for that hop's URL, so credentials
    /// go only where the caller allows. Go instead copies them to the same host and its
    /// subdomains. A body `authorize` cannot withhold, one that carries credentials, stays
    /// with `redirects`: [`Redirects::SameOrigin`] refuses a redirect to any other origin,
    /// a switch from TLS to plain HTTP included, before anything is sent there.
    pub fn follow(
        &self,
        req: &Request<'_>,
        authorize: &dyn Fn(&Url) -> Result<Option<String>, Error>,
        redirects: Redirects,
    ) -> Result<Response<'_>, Error> {
        let mut url = req.url.clone();
        let mut method = req.method;
        let mut body = req.body;
        let mut file = req.file;
        for _ in 0..=MAX_REDIRECTS {
            let authorization = authorize(&url)?;
            let mut headers = req.headers.to_vec();
            if let Some(value) = &authorization {
                headers.push(("Authorization", value));
            }
            let mut response = self.send(&Request {
                method,
                url: &url,
                headers: &headers,
                body,
                file,
            })?;
            let location = match response.status {
                301 | 302 | 303 | 307 | 308 => response.header("location").map(str::to_string),
                _ => None,
            };
            let Some(location) = location else {
                return Ok(response);
            };
            let _ = io::copy(&mut (&mut response).take(DRAIN), &mut io::sink());
            if matches!(response.status, 301..=303) {
                if !matches!(method, "GET" | "HEAD") {
                    method = "GET";
                }
                body = &[];
                file = None;
            }
            let next = url.join(&location)?;
            if redirects == Redirects::SameOrigin && !next.same_origin(req.url) {
                return Err(Error::new(format!(
                    "{}: redirected to {}, another origin, which a request carrying credentials may not follow",
                    req.url.origin(),
                    next.origin()
                )));
            }
            url = next;
        }
        Err(Error::new(format!("{}: stopped after 10 redirects", req.url)))
    }

    /// The request line and fields, refusing any value that could split them.
    fn head(&self, req: &Request<'_>) -> Result<Vec<u8>, Error> {
        let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.url.target());
        let mut fields: Vec<(&str, String)> = vec![
            ("Host", req.url.authority()),
            ("User-Agent", self.user_agent.clone()),
        ];
        fields.extend(req.headers.iter().map(|(n, v)| (*n, v.to_string())));
        if let Some((_, len)) = req.file {
            fields.push(("Content-Length", len.to_string()));
        } else if !req.body.is_empty() || req.method == "POST" || req.method == "PUT" {
            fields.push(("Content-Length", req.body.len().to_string()));
        }
        for (name, value) in fields {
            if !valid_name(name) || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                return Err(Error::new(format!("an invalid {name:?} field")));
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(&value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut head = head.into_bytes();
        head.extend_from_slice(req.body);
        Ok(head)
    }

    fn exchange(&self, mut conn: Conn, req: &Request<'_>, head: &[u8]) -> Result<Response<'_>, Failure> {
        let written = conn.io.get_mut().write_all(head).and_then(|()| {
            if let Some((mut file, len)) = req.file {
                use std::io::{Seek as _, SeekFrom};
                file.seek(SeekFrom::Start(0))?;
                let sent = io::copy(&mut file.take(len), conn.io.get_mut())?;
                if sent != len {
                    return Err(io::Error::other(format!("{sent} of the body's {len} bytes")));
                }
            }
            conn.io.get_mut().flush()
        });
        // A server may answer before it has read the whole request, then close: a 413 or a
        // 401 to an upload. Go's transport reads that answer as the request is written, so
        // where the write fails as the server leaves, the answer it left is read.
        let (head, early) = match written {
            Ok(()) => (read_head(&mut conn, req.url)?, false),
            Err(e) => {
                let left = matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                );
                let sending = || Failure::BeforeResponse(Error::new(format!("{}: sending: {e}", req.url)));
                if !left {
                    return Err(sending());
                }
                (read_head(&mut conn, req.url).map_err(|_| sending())?, true)
            }
        };
        let Head {
            status,
            version,
            fields: headers,
        } = head;
        let framing = framing(req.method, status, version, &headers).map_err(Failure::Other)?;
        let close = match version {
            0 => {
                !has_token(&headers, "connection", "keep-alive") || has_token(&headers, "connection", "close")
            }
            _ => has_token(&headers, "connection", "close"),
        };
        // A response with both framings is suspect (RFC 9112 §6.3); never reuse its connection.
        let both = headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("transfer-encoding"))
            && headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("content-length"));
        conn.stall().map_err(|e| Failure::Other(e.into()))?;
        let mut body = Body {
            conn: Some(conn),
            framing,
            reuse: !close && !both && !early,
            pool: &self.pool,
        };
        body.finish_if_done();
        Ok(Response {
            status,
            url: req.url.clone(),
            headers,
            body,
        })
    }

    fn connect(&self, url: &Url, key: Key) -> Result<Conn, Error> {
        let tcp = dial(url, self.cancel.as_ref())?;
        let _ = tcp.set_nodelay(true);
        // Watched from here, the TLS handshake included.
        let watch = self.cancel.as_ref().map(|c| c.watch(&tcp));
        let stream = match url.scheme() {
            Scheme::Http => Stream::Plain(tcp),
            Scheme::Https => {
                let config = (self.tls)(url)?;
                let host = url.host().trim_start_matches('[').trim_end_matches(']');
                let name =
                    ServerName::try_from(host.to_string()).map_err(|e| Error::new(format!("{url}: {e}")))?;
                let mut tls = ClientConnection::new(config, name)?;
                let mut tcp = tcp;
                let deadline = Instant::now() + HANDSHAKE;
                while tls.is_handshaking() {
                    let left = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|d| !d.is_zero())
                        .ok_or_else(|| {
                            Error::of(
                                ErrorKind::Transient,
                                format!("{url}: the TLS handshake timed out"),
                            )
                        })?;
                    tcp.set_read_timeout(Some(left))?;
                    tcp.set_write_timeout(Some(left))?;
                    tls.complete_io(&mut tcp)
                        .map_err(|e| Error::from(e).context(format!("{url}: TLS handshake")))?;
                }
                Stream::Tls(Box::new(StreamOwned::new(tls, tcp)))
            }
        };
        let conn = Conn {
            key,
            io: BufReader::with_capacity(64 << 10, stream),
            watch,
        };
        // The request's writes are bounded too: a fresh connection would otherwise have
        // none, or what was left of the handshake's.
        conn.stall()?;
        Ok(conn)
    }
}

/// Why an exchange failed: before any response byte (so a GET can be retried), or later.
enum Failure {
    BeforeResponse(Error),
    Other(Error),
}

impl Failure {
    fn into_error(self) -> Error {
        match self {
            Failure::BeforeResponse(e) | Failure::Other(e) => e,
        }
    }
}

/// Connects to `url`'s host, racing its addresses as RFC 8305 §5 describes: families
/// interleaved, the next attempt started 300 ms after the last or as soon as it fails.
/// Gives up at once if `cancel` is cancelled; the attempts under way end on their own.
fn dial(url: &Url, cancel: Option<&Cancel>) -> Result<TcpStream, Error> {
    let host = url.host().trim_start_matches('[').trim_end_matches(']');
    let found: Vec<SocketAddr> = (host, url.port())
        .to_socket_addrs()
        .map_err(|e| Error::new(format!("{}: resolving: {e}", url.host())))?
        .collect();
    let addrs = interleave(found);
    let deadline = Instant::now() + CONNECT;
    let (tx, rx) = mpsc::channel();
    let mut pending = 0usize;
    let mut last = None;
    let wait =
        |pending: &mut usize, last: &mut Option<io::Error>, limit: Duration| match rx.recv_timeout(limit) {
            Ok(Ok(stream)) => Some(stream),
            Ok(Err(e)) => {
                *pending = pending.saturating_sub(1);
                *last = Some(e);
                None
            }
            Err(_) => None,
        };
    for addr in addrs {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("shards-dial".into())
            .spawn(move || {
                let _ = tx.send(TcpStream::connect_timeout(&addr, left));
            });
        match spawned {
            Ok(_) => pending += 1,
            Err(e) => {
                last = Some(e);
                continue;
            }
        }
        let started = Instant::now();
        while let Some(limit) = RACE.checked_sub(started.elapsed()) {
            let before = pending;
            if let Some(stream) = wait(&mut pending, &mut last, limit.min(DIAL_POLL)) {
                return Ok(stream);
            }
            if let Some(cancel) = cancel {
                cancel.check()?;
            }
            // An attempt failed: start the next one now.
            if pending < before {
                break;
            }
        }
    }
    while pending > 0 {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if let Some(stream) = wait(&mut pending, &mut last, left.min(DIAL_POLL)) {
            return Ok(stream);
        }
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
    }
    Err(match last {
        Some(e) => Error::from(e).context(format!("{}: connecting", url.authority())),
        None => Error::of(
            ErrorKind::Transient,
            format!("{}: connecting timed out", url.authority()),
        ),
    })
}

/// Addresses with families alternating, starting with the resolver's first (RFC 8305 §4).
fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let first_v6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (mut a, mut b): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|s| s.is_ipv6() == first_v6);
    let mut out = Vec::with_capacity(a.len() + b.len());
    a.reverse();
    b.reverse();
    loop {
        match (a.pop(), b.pop()) {
            (None, None) => break,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
    out
}

/// A response head: the status, the minor HTTP version, and the fields.
struct Head {
    status: u16,
    version: u8,
    fields: Vec<(String, String)>,
}

/// Reads a response head, skipping 1xx ones, within 10 MiB and 30 s in all.
fn read_head(conn: &mut Conn, url: &Url) -> Result<Head, Failure> {
    let deadline = Instant::now() + HEAD;
    let mut budget = MAX_HEAD;
    let mut first = true;
    loop {
        let mut buf = Vec::new();
        let mut lines = 0;
        let parsed = loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| {
                    Failure::Other(Error::of(
                        ErrorKind::Transient,
                        format!("{url}: no response within 30 s"),
                    ))
                })?;
            conn.set_timeout(left).map_err(|e| Failure::Other(e.into()))?;
            let available = match conn.io.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if first && buf.is_empty() => {
                    return Err(Failure::BeforeResponse(Error::from(e).context(url)));
                }
                Err(e) => {
                    return Err(Failure::Other(
                        Error::from(e).context(format!("{url}: reading the response")),
                    ));
                }
            };
            if available.is_empty() {
                let e = Error::of(
                    ErrorKind::Transient,
                    format!("{url}: the connection closed before a response"),
                );
                return Err(if first && buf.is_empty() {
                    Failure::BeforeResponse(e)
                } else {
                    Failure::Other(e)
                });
            }
            let old = buf.len();
            let take = available.len().min(budget.saturating_sub(old).saturating_add(1));
            let new = available.get(..take).unwrap_or_default();
            lines += new.iter().filter(|&&b| b == b'\n').count();
            buf.extend_from_slice(new);
            // A head ends with an empty line: parsed once one may have come, and as its
            // first bytes come, so a malformed start fails at once. Parsing it again on
            // each read would be quadratic in a head sent a byte at a time.
            let tail = buf.get(old.saturating_sub(2)..).unwrap_or_default();
            if old > 0 && !tail.windows(2).any(|w| w == b"\n\n") && !tail.windows(3).any(|w| w == b"\n\r\n") {
                conn.io.consume(take);
                if buf.len() > budget {
                    return Err(Failure::Other(Error::new(format!(
                        "{url}: the response head passes 10 MiB"
                    ))));
                }
                continue;
            }
            let mut fields = vec![httparse::EMPTY_HEADER; lines.max(1)];
            let mut response = httparse::Response::new(&mut fields);
            match response.parse(&buf) {
                Ok(httparse::Status::Complete(len)) => {
                    conn.io.consume(len.saturating_sub(old));
                    let status = response.code.unwrap_or_default();
                    let version = response.version.unwrap_or_default();
                    let fields = response
                        .headers
                        .iter()
                        .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned()))
                        .collect::<Vec<_>>();
                    budget = budget.saturating_sub(len);
                    break Head {
                        status,
                        version,
                        fields,
                    };
                }
                Ok(httparse::Status::Partial) => {
                    conn.io.consume(take);
                    if buf.len() > budget {
                        return Err(Failure::Other(Error::new(format!(
                            "{url}: the response head passes 10 MiB"
                        ))));
                    }
                }
                Err(e) => {
                    return Err(Failure::Other(Error::new(format!(
                        "{url}: a malformed response head: {e}"
                    ))));
                }
            }
        };
        first = false;
        match parsed.status {
            101 => {
                return Err(Failure::Other(Error::new(format!(
                    "{url}: an unexpected protocol switch"
                ))));
            }
            100..=199 => continue,
            _ => return Ok(parsed),
        }
    }
}

/// The body framing of a response to `method` (Go's `fixLength` and
/// `parseTransferEncoding`).
fn framing(method: &str, status: u16, version: u8, headers: &[(String, String)]) -> Result<Framing, Error> {
    let values = |name: &str| {
        headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim_matches([' ', '\t']))
            .collect::<Vec<_>>()
    };
    // HTTP/1.0 messages have no transfer codings (Go issue 12785).
    let codings = if version >= 1 {
        values("transfer-encoding")
    } else {
        Vec::new()
    };
    let chunked = match codings.as_slice() {
        [] => false,
        [only] if only.eq_ignore_ascii_case("chunked") => true,
        _ => return Err(Error::new(format!("unsupported transfer encoding {codings:?}"))),
    };
    let lengths = values("content-length");
    let length = match lengths.split_first() {
        None => None,
        Some((first, rest)) => {
            if rest.iter().any(|l| l != first) {
                return Err(Error::new(format!(
                    "conflicting Content-Length fields {lengths:?}"
                )));
            }
            // strconv.ParseUint(s, 10, 63): ASCII digits only, below 2^63.
            let valid = !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit());
            match first.parse::<u64>().ok().filter(|n| valid && *n < 1 << 63) {
                Some(n) => Some(n),
                None => return Err(Error::new(format!("a bad Content-Length {first:?}"))),
            }
        }
    };
    if method == "HEAD" || matches!(status, 100..=199 | 204 | 304) {
        return Ok(Framing::Length(0));
    }
    Ok(match (chunked, length) {
        (true, _) => Framing::Chunked(Chunked::default()),
        (false, Some(n)) => Framing::Length(n),
        (false, None) => Framing::Close,
    })
}

/// Whether a comma-separated field `name` lists `token` (RFC 9110 §5.6.1).
fn has_token(headers: &[(String, String)], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| v.split(','))
        .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(token))
}

/// A field name is a token (RFC 9110 §5.1, §5.6.2).
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

#[derive(Debug)]
enum Framing {
    Length(u64),
    Chunked(Chunked),
    Close,
}

#[derive(Debug, Default)]
struct Chunked {
    /// Bytes left in the current chunk.
    left: u64,
    /// Whether the CRLF after a chunk's data is due.
    check_end: bool,
    /// Non-data bytes read beyond what Go allows for free.
    excess: u64,
    done: bool,
}

#[derive(Debug)]
struct Body<'c> {
    conn: Option<Conn>,
    framing: Framing,
    reuse: bool,
    pool: &'c Pool,
}

impl Body<'_> {
    /// Hands the connection back to the pool once the body is over, if it may be reused.
    fn finish_if_done(&mut self) {
        let done = match &self.framing {
            Framing::Length(n) => *n == 0,
            Framing::Chunked(c) => c.done,
            Framing::Close => false,
        };
        // A connection that may not be reused closes as it drops, and so does one holding
        // bytes past the body: they would be read as the next response.
        if done
            && let Some(conn) = self.conn.take()
            && self.reuse
            && conn.io.buffer().is_empty()
        {
            self.pool.put(conn);
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(conn) = self.conn.as_mut() else {
            return Ok(0);
        };
        if buf.is_empty() {
            return Ok(0);
        }
        let n = match &mut self.framing {
            Framing::Length(left) => {
                if *left == 0 {
                    0
                } else {
                    let want = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                    let n = conn.io.read(buf.get_mut(..want).unwrap_or_default())?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the body ended early",
                        ));
                    }
                    *left = left.saturating_sub(n as u64);
                    n
                }
            }
            Framing::Chunked(state) => read_chunked(&mut conn.io, state, buf)?,
            Framing::Close => {
                let n = conn.io.read(buf)?;
                if n == 0 {
                    self.conn = None;
                }
                n
            }
        };
        self.finish_if_done();
        Ok(n)
    }
}

/// One read of a chunked body (Go's `chunkedReader.Read`).
fn read_chunked(io: &mut BufReader<Stream>, c: &mut Chunked, buf: &mut [u8]) -> io::Result<usize> {
    let bad = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_string());
    loop {
        if c.done {
            return Ok(0);
        }
        if c.check_end {
            let mut crlf = [0u8; 2];
            io.read_exact(&mut crlf)?;
            if &crlf != b"\r\n" {
                return Err(bad("malformed chunked encoding"));
            }
            c.check_end = false;
        }
        if c.left == 0 {
            let line = read_line(io, MAX_CHUNK_LINE)?;
            // Only a CR right before the LF (RFC 9112 erratum 7633).
            let line = match line.strip_suffix(b"\r\n") {
                Some(l) if !l.contains(&b'\r') => l,
                _ => return Err(bad("a chunk line must end in CRLF")),
            };
            // The line, and the CRLF after the chunk's data.
            c.excess = c.excess.saturating_add(line.len() as u64 + 2);
            // Trailing whitespace goes first, then the extension: "5 ;x" is refused.
            let mut end = line.len();
            while end > 0 && matches!(line.get(end - 1), Some(b' ' | b'\t')) {
                end -= 1;
            }
            let trimmed = line.get(..end).unwrap_or_default();
            let size = trimmed.split(|&b| b == b';').next().unwrap_or_default();
            c.left = parse_hex(size).ok_or_else(|| bad("a bad chunk size"))?;
            c.excess = c
                .excess
                .saturating_sub(16u64.saturating_add(c.left.saturating_mul(2)));
            if c.excess > 16 * 1024 {
                return Err(bad("chunked encoding contains too much non-data"));
            }
            if c.left == 0 {
                read_trailer(io)?;
                c.done = true;
                return Ok(0);
            }
            continue;
        }
        let want = buf.len().min(usize::try_from(c.left).unwrap_or(usize::MAX));
        let n = io.read(buf.get_mut(..want).unwrap_or_default())?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the chunked body ended early",
            ));
        }
        c.left = c.left.saturating_sub(n as u64);
        c.check_end = c.left == 0;
        return Ok(n);
    }
}

/// Go's `parseHexUint`: 1 to 16 hex digits.
fn parse_hex(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    digits.iter().try_fold(0u64, |n, &d| {
        let v = char::from(d).to_digit(16)?;
        Some(n << 4 | u64::from(v))
    })
}

/// Skips a trailer section, which must end within 4 KiB, as Go bounds it by its reader's
/// buffer. Lines may end in LF alone, as textproto reads them.
fn read_trailer(io: &mut BufReader<Stream>) -> io::Result<()> {
    let mut left = MAX_TRAILER;
    loop {
        let line = read_line(io, left)?;
        left = left.saturating_sub(line.len());
        if line == b"\r\n" || line == b"\n" {
            return Ok(());
        }
    }
}

/// A line through its LF, of at most `max` bytes.
fn read_line(io: &mut BufReader<Stream>, max: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = match io.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the body ended mid-line",
            ));
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if line.len().saturating_add(take) > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a line is too long"));
        }
        line.extend_from_slice(available.get(..take).unwrap_or_default());
        io.consume(take);
        if done {
            return Ok(line);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    scheme: Scheme,
    host: String,
    port: u16,
}

impl Key {
    fn of(url: &Url) -> Key {
        Key {
            scheme: url.scheme(),
            host: url.host().to_string(),
            port: url.port(),
        }
    }
}

#[derive(Debug)]
enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

#[derive(Debug)]
struct Conn {
    key: Key,
    io: BufReader<Stream>,
    /// While the connection is in use: idle, it is no request's to cancel.
    watch: Option<Watch>,
}

impl Conn {
    /// Bounds each read and write by `d`. macOS refuses any option on a socket shut down
    /// both ways, as one the server has reset is, with EINVAL (xnu-11417.101.15
    /// bsd/kern/uipc_socket.c, sosetoptlock): its reads return what is buffered, then its
    /// end, and its writes fail, at once, so it needs no bound, and the exchange goes on
    /// to find out what the server sent.
    fn set_timeout(&self, d: Duration) -> io::Result<()> {
        let tcp = self.io.get_ref().tcp();
        for set in [TcpStream::set_read_timeout, TcpStream::set_write_timeout] {
            match set(tcp, Some(d)) {
                Err(e) if e.kind() == io::ErrorKind::InvalidInput && e.raw_os_error().is_some() => {}
                result => result?,
            }
        }
        Ok(())
    }

    /// From here on, any read or write that makes no progress for 30 s fails.
    fn stall(&self) -> io::Result<()> {
        self.set_timeout(STALL)
    }

    /// Whether an idle connection can carry a request: the server has sent nothing since,
    /// neither a close nor bytes nobody asked for. Go's transport drops a connection that
    /// receives an unsolicited response in the same way.
    fn quiet(&self) -> bool {
        let tcp = self.io.get_ref().tcp();
        if tcp.set_nonblocking(true).is_err() {
            return false;
        }
        let quiet = matches!(tcp.peek(&mut [0u8; 1]), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
        tcp.set_nonblocking(false).is_ok() && quiet
    }
}

/// Idle connections, newest last.
#[derive(Debug, Default)]
struct Pool {
    idle: Mutex<Vec<(Instant, Conn)>>,
}

impl Pool {
    fn take(&self, key: &Key) -> Option<Conn> {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.retain(|(since, conn)| since.elapsed() < IDLE && conn.quiet());
        let i = idle.iter().rposition(|(_, c)| c.key == *key)?;
        Some(idle.remove(i).1)
    }

    fn put(&self, mut conn: Conn) {
        conn.watch = None;
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.push((Instant::now(), conn));
        if idle.len() > MAX_IDLE {
            idle.remove(0);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    use crate::testing::{After, registry, serve};
    use crate::tls::client_config;

    fn plain() -> Client {
        Client::new(
            Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
            "shards-test",
        )
    }

    fn fetch(client: &Client, method: &str, url: &Url) -> Result<(u16, Vec<u8>), Error> {
        let mut response = client.send(&Request {
            method,
            url,
            headers: &[],
            body: &[],
            file: None,
        })?;
        let mut body = Vec::new();
        response
            .read_to_end(&mut body)
            .map_err(|e| Error::new(e.to_string()))?;
        Ok((response.status, body))
    }

    fn at(scheme: &str, host: &str, port: u16) -> Url {
        Url::parse(&format!("{scheme}://{host}:{port}/v2/")).unwrap()
    }

    /// One response on its own connection, and what reading it gives.
    fn outcome(response: &str) -> Result<Vec<u8>, Error> {
        let server = serve(None, vec![(response.as_bytes().to_vec(), After::Close)]);
        fetch(&plain(), "GET", &at("http", "127.0.0.1", server.port)).map(|(_, body)| body)
    }

    #[test]
    fn bodies_are_framed_as_go_frames_them_and_connections_reused() {
        let script = [
            ("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", After::Keep),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;name=value\r\nhello\r\n6\r\n world\r\n0\r\nTrailer-Field: x\r\n\r\n",
                After::Keep,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\nok",
                After::Keep,
            ),
            (
                "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 204 No Content\r\nContent-Length: 7\r\n\r\n",
                After::Keep,
            ),
            ("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n", After::Keep),
            ("HTTP/1.1 200 OK\r\n\r\nuntil close", After::Close),
            ("HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok", After::Keep),
            ("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nnew", After::Keep),
        ];
        let server = serve(
            None,
            script.iter().map(|(r, a)| (r.as_bytes().to_vec(), *a)).collect(),
        );
        let url = at("http", "127.0.0.1", server.port);
        let client = plain();
        let get = |method| fetch(&client, method, &url).unwrap();
        assert_eq!(get("GET"), (200, b"hello".to_vec()));
        assert_eq!(get("GET"), (200, b"hello world".to_vec()));
        assert_eq!(get("GET"), (200, b"ok".to_vec()));
        assert_eq!(get("GET"), (204, Vec::new()), "1xx skipped; 204 has no body");
        assert_eq!(get("HEAD"), (200, Vec::new()), "HEAD has no body");
        assert_eq!(server.accepted(), 1, "one connection so far");
        assert_eq!(get("GET"), (200, b"until close".to_vec()));
        assert_eq!(get("GET"), (200, b"ok".to_vec()), "HTTP/1.0, not kept alive");
        assert_eq!(get("GET"), (200, b"new".to_vec()));
        assert_eq!(server.accepted(), 3);
    }

    #[test]
    fn malformed_framing_is_refused() {
        let chunked = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let long_line = format!("{chunked}5;{}\r\nhello\r\n0\r\n\r\n", "x".repeat(5000));
        let long_trailer = format!("{chunked}0\r\nT: {}\r\n\r\n", "y".repeat(5000));
        for (response, why) in [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nok",
                "conflicting lengths",
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: +2\r\n\r\nok",
                "a signed length",
            ),
            ("HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\nok", "an empty length"),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort",
                "a short body",
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
                "a coding besides chunked",
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
                "two Transfer-Encoding fields",
            ),
            (&format!("{chunked}5\nhello\r\n0\r\n\r\n"), "a bare LF"),
            (&format!("{chunked}5\r\r\nhello\r\n0\r\n\r\n"), "a stray CR"),
            (
                &format!("{chunked}00000000000000005\r\nhello\r\n0\r\n\r\n"),
                "17 hex digits",
            ),
            (
                &format!("{chunked}5 ;x\r\nhello\r\n0\r\n\r\n"),
                "space before an extension",
            ),
            (
                &format!("{chunked}5\r\nhelloXX0\r\n\r\n"),
                "no CRLF after the data",
            ),
            (&format!("{chunked}5\r\nhel"), "a cut chunk"),
            (&long_line, "a 5000-byte chunk line"),
            (&long_trailer, "a 5000-byte trailer"),
            ("HTTP/1.1 101 Switching Protocols\r\n\r\n", "a protocol switch"),
            ("HTTP/1.1 2000 OK\r\n\r\n", "a malformed status line"),
        ] {
            assert!(outcome(response).is_err(), "{why}");
        }
        // Go takes both framings, and the chunks win.
        let both = "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(outcome(both).unwrap(), b"hello");
    }

    /// A cancel ends a request at once, whatever it waits for: a head that never comes, a
    /// body that stops halfway; and a cancelled client connects nowhere (audit A07). Each
    /// wait would otherwise last 30 s.
    #[test]
    fn a_cancel_ends_requests_at_once() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = at("http", "127.0.0.1", port);
        let server = std::thread::spawn(move || {
            // The first says nothing; the second sends a head and 10 of its 1000 bytes.
            let (silent, _) = listener.accept().unwrap();
            let (mut halfway, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = halfway.read(&mut request);
            halfway
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n0123456789")
                .unwrap();
            listener.set_nonblocking(true).unwrap();
            (silent, halfway, listener)
        });
        let cancelled_within = |client: &Client, cancel: &Cancel| {
            std::thread::scope(|scope| {
                let asked = scope.spawn(|| fetch(client, "GET", &url));
                std::thread::sleep(Duration::from_millis(100));
                let t0 = Instant::now();
                cancel.cancel();
                let e = asked.join().unwrap().unwrap_err();
                (e, t0.elapsed())
            })
        };
        let cancel = Cancel::new();
        let (e, took) = cancelled_within(&plain().cancelled_by(cancel.clone()), &cancel);
        assert_eq!(e.kind(), ErrorKind::Cancelled, "{e}");
        assert!(took < Duration::from_secs(2), "{took:?}");
        let cancel = Cancel::new();
        let (e, took) = cancelled_within(&plain().cancelled_by(cancel.clone()), &cancel);
        assert!(took < Duration::from_secs(2), "{took:?}: {e}");
        let (_silent, _halfway, listener) = server.join().unwrap();
        // Cancelled already: nothing is sent, and no connection made.
        let e = fetch(&plain().cancelled_by(cancel), "GET", &url).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Cancelled, "{e}");
        assert!(listener.accept().is_err(), "a cancelled client connected");
    }

    /// A connection the server has reset takes its timeouts without error, and its reads
    /// end at once: macOS refused them with EINVAL, which failed a request that should have
    /// been sent again on a new connection (the flaky `a_stale_pooled_connection_is_replaced`).
    #[test]
    fn a_reset_connection_takes_its_timeouts() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (server, _) = listener.accept().unwrap();
        // Closing with bytes it never read, the server resets the connection.
        tcp.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        drop(server);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut byte = [0u8; 1];
            tcp.set_nonblocking(true).unwrap();
            let seen = tcp.peek(&mut byte);
            tcp.set_nonblocking(false).unwrap();
            if !matches!(&seen, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
                break;
            }
            assert!(Instant::now() < deadline, "the reset never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        let conn = Conn {
            key: Key::of(&at("http", "127.0.0.1", port)),
            io: BufReader::new(Stream::Plain(tcp)),
            watch: None,
        };
        conn.set_timeout(Duration::from_secs(30)).unwrap();
        conn.stall().unwrap();
        let t0 = Instant::now();
        let mut conn = conn;
        let mut rest = Vec::new();
        let _ = conn.io.read_to_end(&mut rest);
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    }

    /// A fresh connection's reads and writes are bounded from the start: the request is
    /// written within them too, which nothing bounded before its response's head.
    #[test]
    fn fresh_connections_bound_their_writes() {
        let server = serve(None, vec![]);
        let url = at("http", "127.0.0.1", server.port);
        let conn = plain().connect(&url, Key::of(&url)).unwrap();
        let tcp = conn.io.get_ref().tcp();
        assert_eq!(tcp.write_timeout().unwrap(), Some(STALL));
        assert_eq!(tcp.read_timeout().unwrap(), Some(STALL));
    }

    /// A server that answers an upload before reading it, then leaves, is heard: its 413,
    /// not the broken pipe, as Go's transport reads an answer while it writes.
    #[test]
    fn an_answer_before_the_body_is_read_is_heard() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                tcp.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            tcp.write_all(b"HTTP/1.1 413 Request Entity Too Large\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            // Gone with the body unread: the client's writes fail.
        });
        let path = std::env::temp_dir().join(format!("shards-early-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 64 << 20]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let url = at("http", "127.0.0.1", port);
        let client = plain();
        let status = client
            .send(&Request {
                method: "PUT",
                url: &url,
                headers: &[],
                body: &[],
                file: Some((&file, 64 << 20)),
            })
            .map(|r| r.status);
        let _ = std::fs::remove_file(&path);
        server.join().unwrap();
        assert_eq!(status.unwrap(), 413);
    }

    /// A head sent a byte at a time, its lines ended by LF alone, reads as one sent whole.
    #[test]
    fn a_head_in_pieces_is_read_whole() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            let _ = tcp.set_nodelay(true);
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                tcp.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            for b in b"HTTP/1.1 200 OK\nX-One: 1\nContent-Length: 2\n\nhi" {
                tcp.write_all(&[*b]).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let url = at("http", "127.0.0.1", port);
        let (status, body) = fetch(&plain(), "GET", &url).unwrap();
        server.join().unwrap();
        assert_eq!((status, body.as_slice()), (200, &b"hi"[..]));
    }

    #[test]
    fn a_stale_pooled_connection_is_replaced() {
        let server = serve(
            None,
            vec![
                (
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
                    After::Close,
                ),
                (
                    b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nnew".to_vec(),
                    After::Keep,
                ),
            ],
        );
        let url = at("http", "127.0.0.1", server.port);
        let client = plain();
        assert_eq!(fetch(&client, "GET", &url).unwrap().1, b"ok");
        assert_eq!(fetch(&client, "GET", &url).unwrap().1, b"new");
        assert_eq!(server.accepted(), 2);
    }

    #[test]
    fn fields_that_could_split_the_head_are_refused() {
        let url = at("http", "127.0.0.1", 9);
        for headers in [
            [("Authorization", "Bearer x\r\nInjected: 1")],
            [("Bad Name", "x")],
            [("X", "a\0b")],
        ] {
            let request = Request {
                method: "GET",
                url: &url,
                headers: &headers,
                body: &[],
                file: None,
            };
            let e = plain().send(&request).unwrap_err();
            assert!(e.to_string().contains("invalid"), "{e}");
        }
    }

    #[test]
    fn https_exchanges_share_one_tls_connection() {
        let (ca, server) = registry(&[&rustls::version::TLS13]);
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec();
        let listening = serve(Some(server), vec![(ok.clone(), After::Keep), (ok, After::Keep)]);
        let client = Client::new(
            Box::new(move |_| client_config(vec![ca.clone()], None)),
            "shards-test",
        );
        // localhost may resolve to ::1 first, where nothing listens: the race moves on.
        let url = at("https", "localhost", listening.port);
        assert_eq!(fetch(&client, "GET", &url).unwrap(), (200, b"ok".to_vec()));
        assert_eq!(fetch(&client, "GET", &url).unwrap(), (200, b"ok".to_vec()));
        assert_eq!(listening.accepted(), 1);
    }

    #[test]
    fn address_families_alternate() {
        let v6 = |n: u16| SocketAddr::from((Ipv6Addr::LOCALHOST, n));
        let v4 = |n: u16| SocketAddr::from((Ipv4Addr::LOCALHOST, n));
        assert_eq!(
            interleave(vec![v6(1), v6(2), v4(3), v4(4)]),
            [v6(1), v4(3), v6(2), v4(4)]
        );
        assert_eq!(interleave(vec![v4(1), v6(2), v6(3)]), [v4(1), v6(2), v6(3)]);
    }
}
