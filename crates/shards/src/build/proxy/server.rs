//! The proxy's server (D110): its Unix socket, a step's session on it, and each of the
//! step's connections, plain or tunnelled, served on threads of the step's.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use shards_dockerfile::url as gourl;
use shards_registry::http::{Cancel, Client, Patience, Request};
use shards_registry::url::{Scheme, Url};

use sha2::Digest as _;

use super::ca;
use super::capture::{self, Capture};
use super::head::{self, Framing, Head};
use super::sniff;

/// A body's digest as BuildKit writes one: `sha256:` and its hex.
fn digest_of(hasher: sha2::Sha256) -> String {
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// The gateway's port a step's proxy variables name.
pub const PORT: u16 = 3128;

/// A request body kept in memory up to this; past it, in a file of the proxy's own.
const IN_MEMORY: usize = 64 << 10;

/// Whether a request may go to `ip`: never the host itself (its loopback, unspecified,
/// link-local, multicast and broadcast addresses, nor IPv4-compatible IPv6), as the
/// builder's network keeps a step from them where it has the network (D31).
pub fn reachable(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_unspecified()
                && !v4.is_link_local()
                && !v4.is_multicast()
                && !v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_unicast_link_local()
                && !v6.is_multicast()
                && v6.segments().get(..6) != Some(&[0; 6])
        }
    }
}

/// `net.SplitHostPort`'s host, else the whole without brackets (BuildKit's `stripPort`).
fn strip_port(hostport: &str) -> &str {
    split_host(hostport).unwrap_or_else(|| hostport.trim_matches(['[', ']']))
}

/// `net.SplitHostPort`'s host, where it splits.
fn split_host(hostport: &str) -> Option<&str> {
    let i = hostport.rfind(':')?;
    let (host, j, k) = if hostport.starts_with('[') {
        let end = hostport.find(']')?;
        if end + 1 != i {
            return None;
        }
        (hostport.get(1..end)?, 1, end + 1)
    } else {
        let host = hostport.get(..i)?;
        if host.contains(':') {
            return None;
        }
        (host, 0, 0)
    };
    if hostport.get(j..)?.contains('[') || hostport.get(k..)?.contains(']') {
        return None;
    }
    Some(host)
}

/// The URL BuildKit's handler checks and fetches for a request: its own where absolute;
/// else `http://` and its `Host` (the URL's own host, else its field, as Go's server reads
/// it); in a tunnel, `https://` and the tunnel's host whatever the request says.
fn request_url(head: &Head, tunnel: Option<&str>) -> Result<gourl::Url, Vec<u8>> {
    let mut u = gourl::parse_request_uri(head.target.as_bytes())?;
    match tunnel {
        Some(host) => {
            u.scheme = b"https".to_vec();
            u.host = host.as_bytes().to_vec();
        }
        None if u.scheme.is_empty() => {
            u.scheme = b"http".to_vec();
            if u.host.is_empty() {
                u.host = head.get("Host").unwrap_or_default().as_bytes().to_vec();
            }
        }
        None => {}
    }
    Ok(u)
}

/// A CONNECT's host as Go's server reads it: the authority it names, else its `Host`.
fn connect_host(head: &Head) -> String {
    let raw = if head.target.starts_with('/') {
        head.target.clone()
    } else {
        format!("http://{}", head.target)
    };
    match gourl::parse_request_uri(raw.as_bytes()) {
        Ok(u) if !u.host.is_empty() => String::from_utf8_lossy(&u.host).into_owned(),
        _ => head.get("Host").unwrap_or_default().to_string(),
    }
}

/// `textproto.CanonicalMIMEHeaderKey`, for a response's fields as Go's client keeps them.
fn canonical(key: &str) -> String {
    if !key
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
    {
        return key.to_string();
    }
    let mut upper = true;
    key.chars()
        .map(|c| {
            let c = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            c
        })
        .collect()
}

/// A response's fields as they are passed on: keys canonical and sorted, as Go writes a
/// header (`Header.WriteSubset`), without those that frame a message or end a hop, which
/// this proxy writes itself: `Connection` and each field it names, and the rest RFC 9110
/// §7.6.1 says a proxy removes (where BuildKit's passes them on, D110); `keep` says which
/// others stay.
fn passed_on(fields: &[(String, String)], keep: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    const HOP: [&str; 8] = [
        "Connection",
        "Keep-Alive",
        "Proxy-Connection",
        "Proxy-Authenticate",
        "Te",
        "Trailer",
        "Transfer-Encoding",
        "Upgrade",
    ];
    let named: Vec<String> = fields
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Connection"))
        .flat_map(|(_, v)| v.split(','))
        .map(|t| canonical(t.trim_matches([' ', '\t'])))
        .collect();
    let mut out: Vec<(String, String)> = fields
        .iter()
        .map(|(k, v)| (canonical(k), v.clone()))
        .filter(|(k, _)| !HOP.contains(&k.as_str()) && !named.contains(k) && keep(k))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Whether a response to `method` with `status` has a body (`bodyAllowedForStatus`, and
/// HEAD's none).
fn has_body(method: &str, status: u16) -> bool {
    method != "HEAD" && !matches!(status, 100..=199 | 204 | 304)
}

/// Now, as an HTTP date (`http.TimeFormat`).
fn http_date() -> String {
    let now = time::OffsetDateTime::now_utc();
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS.get(usize::from(now.weekday().number_days_from_monday()))
            .copied()
            .unwrap_or("Mon"),
        now.day(),
        MONTHS
            .get(usize::from(u8::from(now.month())) - 1)
            .copied()
            .unwrap_or("Jan"),
        now.year(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// A request's body, held to be sent on.
enum Spool {
    Memory(Vec<u8>),
    File {
        file: std::fs::File,
        path: PathBuf,
        size: u64,
    },
}

impl Drop for Spool {
    fn drop(&mut self) {
        if let Spool::File { path, .. } = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Why a request's body was not held: the disk keeps no room for more of it, or its
/// reading or writing failed, and how.
enum Spooled {
    NoRoom,
    Failed(String),
}

/// A new file of the proxy's own for a request's body (mode 0600, a name no other has).
fn spool_file(dir: &Path) -> io::Result<(std::fs::File, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt as _;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("body-{n}"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    Ok((file, path))
}

/// Reads and drops what is left of a request's body BuildKit's handler never read, as Go's
/// server drops it to keep the connection: whether it may be kept (at most 256 KiB left,
/// read whole).
fn discard<R: BufRead>(reader: &mut R, framing: Framing) -> bool {
    if let Framing::Length(n) = framing
        && n >= head::DISCARD
    {
        return false;
    }
    let mut body = head::Body::new(reader, framing);
    let mut sink = vec![0u8; 16 << 10];
    let mut read = 0u64;
    loop {
        match body.read(&mut sink) {
            Ok(0) => return body.ended(),
            Ok(n) => {
                read += n as u64;
                if read > head::DISCARD {
                    return false;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// `http.Error`'s response, as Go's server writes it: the text and a newline, as plain
/// text, never sniffed, of known length; `forced` closes the connection after it. Whether
/// the connection goes on.
fn write_error(out: &mut impl Write, head: &Head, status: u16, text: &str, forced: bool) -> bool {
    let body = format!("{text}\n");
    let (close, connection) = keep_alive(head, true, forced);
    let mut s = String::new();
    s.push_str(&format!(
        "HTTP/1.{} {status} {}\r\n",
        u8::from(head.at_least(1, 1)),
        head::status_text(status)
    ));
    s.push_str("Content-Type: text/plain; charset=utf-8\r\nX-Content-Type-Options: nosniff\r\n");
    s.push_str(&format!(
        "Date: {}\r\nContent-Length: {}\r\n",
        http_date(),
        body.len()
    ));
    if let Some(c) = connection {
        s.push_str(&format!("Connection: {c}\r\n"));
    }
    s.push_str("\r\n");
    // A HEAD's response has its length, never its body.
    if head.method != "HEAD" {
        s.push_str(&body);
    }
    out.write_all(s.as_bytes()).and_then(|()| out.flush()).is_ok() && !close
}

/// The most of an `OPTIONS *` request's body Go's server reads (`globalOptionsHandler`).
const OPTIONS_BODY: u64 = 4 << 10;

/// `OPTIONS *`, which Go's server answers itself (`globalOptionsHandler`), so BuildKit's
/// proxy never sees it: `200` of no body, with up to 4 KiB of the request's body read,
/// past which it closes (MaxBytesReader's `requestTooLarge`, its `Connection` field among
/// the handler's). Nothing goes anywhere, so nothing is asked of the policies. Whether the
/// connection goes on.
fn options<R: BufRead>(
    out: &mut impl Write,
    head: &Head,
    reader: &mut R,
    framing: Framing,
    continues: bool,
) -> bool {
    let (mut too_large, mut failed) = (false, false);
    if framing != Framing::Length(0) {
        // Its first read of the body asks for it, as expectContinueReader does.
        if continues && head.at_least(1, 1) {
            let _ = out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = out.flush();
        }
        let mut body = head::Body::new(reader, framing);
        let mut sink = [0u8; 4096];
        let mut read = 0u64;
        loop {
            match body.read(&mut sink) {
                Ok(0) => break,
                Ok(n) => {
                    read += n as u64;
                    if read > OPTIONS_BODY {
                        too_large = true;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // A body of known length cut short is done with, as Go's `body` sees its
                // end; a chunked one that fails, not (its error stays).
                Err(_) => {
                    failed = framing == Framing::Chunked;
                    break;
                }
            }
        }
    }
    let mut s = format!("HTTP/1.{} 200 OK\r\n", u8::from(head.at_least(1, 1)));
    if too_large {
        s.push_str("Connection: close\r\n");
    }
    s.push_str(&format!("Content-Length: 0\r\nDate: {}\r\n", http_date()));
    let (close, connection) = keep_alive(head, true, failed);
    if let Some(c) = connection.filter(|_| !too_large) {
        s.push_str(&format!("Connection: {c}\r\n"));
    }
    s.push_str("\r\n");
    out.write_all(s.as_bytes()).and_then(|()| out.flush()).is_ok() && !close && !too_large
}

/// Go's server's choice after a response (`chunkWriter.writeHeader`): whether the
/// connection closes, and the `Connection` field that says what it does. An HTTP/1.0
/// client that asked to be kept alive (its first such field alone read,
/// `wants10KeepAlive`) is, for a response of known length; any other HTTP/1.0 client is
/// not, nor one that asked to close, nor where `forced` (a body left unread).
fn keep_alive(head: &Head, length_known: bool, forced: bool) -> (bool, Option<&'static str>) {
    let wants10 = (head.major, head.minor) == (1, 0)
        && head.get("Connection").is_some_and(|c| {
            c.split(',')
                .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case("keep-alive"))
        });
    if wants10 && length_known && !forced {
        return (false, Some("keep-alive"));
    }
    let close = forced || !head.at_least(1, 1) || head.wants_close();
    (close, (close && head.at_least(1, 1)).then_some("close"))
}

/// Passes a response on to a plain request's client as Go's server writes BuildKit's
/// handler's (`copyHeader`, `WriteHeader`, then `io.Copy`, which is the response's
/// `ReadFrom`: the body's first 512 bytes, then the head): the status's own words, the
/// fields sorted, a `Date` where it has none, its length where known or the whole of one
/// shorter than 512 bytes, else chunked (HTTP/1.1) or to the connection's end, and what it
/// is where nothing says (`DetectContentType`). Whether the connection goes on, whether the
/// body was read whole, and its digest.
fn pass_on<W: Write>(
    out: &mut W,
    head: &Head,
    response: &mut shards_registry::http::Response<'_>,
) -> (bool, bool, String) {
    let status = response.status;
    let body = has_body(&head.method, status);
    let fields = passed_on(response.fields(), |k| match status {
        304 => !matches!(k, "Content-Type" | "Content-Length"),
        _ if !body && head.method != "HEAD" => k != "Content-Length",
        _ => true,
    });
    let chunked_upstream = response
        .headers("transfer-encoding")
        .any(|v| v.trim().eq_ignore_ascii_case("chunked"));
    let length = (!chunked_upstream)
        .then(|| {
            response
                .header("content-length")
                .and_then(|l| l.trim().parse::<u64>().ok())
        })
        .flatten();
    let mut fields = fields;
    if chunked_upstream {
        fields.retain(|(k, _)| k != "Content-Length");
    }
    let mut hasher = sha2::Sha256::new();
    let mut whole = true;
    // The body's first bytes, before the head: all of a short one.
    let mut first = Vec::new();
    let mut drained = !body;
    if body {
        let mut buf = [0u8; sniff::LEN];
        while first.len() < sniff::LEN {
            match response.read(buf.get_mut(..sniff::LEN - first.len()).unwrap_or_default()) {
                Ok(0) => {
                    drained = true;
                    break;
                }
                Ok(n) => first.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    whole = false;
                    drained = true;
                    break;
                }
            }
        }
    }
    let known = match length {
        Some(n) => Some(n),
        None if drained && body => Some(first.len() as u64),
        None => None,
    };
    let chunked = body && known.is_none() && head.at_least(1, 1);
    // Sniffed where the fields say neither what it is nor how it is encoded.
    let sniffed = (body
        && !first.is_empty()
        && !fields.iter().any(|(k, _)| k == "Content-Type")
        && fields
            .iter()
            .find(|(k, _)| k == "Content-Encoding")
            .is_none_or(|(_, v)| v.is_empty()))
    .then(|| sniff::content_type(&first));
    let (mut close, connection) = keep_alive(head, !body || known.is_some(), false);
    let mut s = format!(
        "HTTP/1.{} {status} {}\r\n",
        u8::from(head.at_least(1, 1)),
        match head::status_text(status) {
            "" => format!("status code {status}"),
            t => t.to_string(),
        }
    );
    for (k, v) in &fields {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    if !fields.iter().any(|(k, _)| k == "Date") {
        s.push_str(&format!("Date: {}\r\n", http_date()));
    }
    if body
        && length.is_none()
        && let Some(n) = known
    {
        s.push_str(&format!("Content-Length: {n}\r\n"));
    }
    if let Some(t) = sniffed {
        s.push_str(&format!("Content-Type: {t}\r\n"));
    }
    if let Some(c) = connection {
        s.push_str(&format!("Connection: {c}\r\n"));
    }
    if chunked {
        s.push_str("Transfer-Encoding: chunked\r\n");
    }
    s.push_str("\r\n");
    if out.write_all(s.as_bytes()).is_err() {
        return (false, false, String::new());
    }
    let mut sent = true;
    let send = |out: &mut W, data: &[u8]| -> bool {
        if data.is_empty() {
            return true;
        }
        if chunked {
            write!(out, "{:x}\r\n", data.len())
                .and_then(|()| out.write_all(data))
                .and_then(|()| out.write_all(b"\r\n"))
                .is_ok()
        } else {
            out.write_all(data).is_ok()
        }
    };
    hasher.update(&first);
    sent &= send(out, &first);
    if body && !drained && sent {
        let mut buf = vec![0u8; 32 << 10];
        loop {
            match response.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf.get(..n).unwrap_or_default();
                    hasher.update(data);
                    if !send(out, data) {
                        sent = false;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    whole = false;
                    break;
                }
            }
        }
    }
    // A body cut short ends the connection: its client sees it short, never whole.
    if !whole {
        close = true;
    } else if chunked && sent {
        sent = out.write_all(b"0\r\n\r\n").is_ok();
    }
    sent &= out.flush().is_ok();
    let digest = digest_of(hasher);
    (sent && !close, whole && sent, digest)
}

/// Writes a response into a tunnel as BuildKit writes one (`prepareMITMResponse`, then
/// `Response.Write`): the request's version, the upstream's own words, `Connection: close`
/// where the tunnel ends after it, its length or chunked, then the fields sorted. Whether
/// the tunnel goes on, whether the body was read whole, and its digest.
fn write_through_tunnel<W: Write>(
    out: &mut W,
    head: &Head,
    response: &mut shards_registry::http::Response<'_>,
) -> (bool, bool, String) {
    let status = response.status;
    let body = has_body(&head.method, status);
    let chunked_upstream = response
        .headers("transfer-encoding")
        .any(|v| v.trim().eq_ignore_ascii_case("chunked"));
    let length = (!chunked_upstream)
        .then(|| {
            response
                .header("content-length")
                .and_then(|l| l.trim().parse::<u64>().ok())
        })
        .flatten();
    let upstream_close = response
        .headers("connection")
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case("close"));
    // Unknown length, not chunked: the end of the tunnel ends the body; a status of no
    // body has none to end (Go's transport says its length is 0).
    let close = upstream_close
        || head.wants_close()
        || !head.at_least(1, 1)
        || (length.is_none() && !chunked_upstream && has_body("GET", status));
    let chunked = chunked_upstream && body;
    let reason = response.reason().to_string();
    let words = if reason.is_empty() {
        head::status_text(status).to_string()
    } else {
        reason
    };
    let mut s = format!("HTTP/{}.{} {status:03} {words}\r\n", head.major, head.minor);
    if close {
        s.push_str("Connection: close\r\n");
    }
    match length {
        Some(n) if n > 0 || matches!(head.method.as_str(), "POST" | "PUT" | "PATCH") => {
            s.push_str(&format!("Content-Length: {n}\r\n"));
        }
        _ if chunked => s.push_str("Transfer-Encoding: chunked\r\n"),
        _ => {}
    }
    for (k, v) in passed_on(response.fields(), |k| k != "Content-Length") {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    if length == Some(0)
        && !matches!(head.method.as_str(), "POST" | "PUT" | "PATCH")
        && has_body("GET", status)
    {
        s.push_str("Content-Length: 0\r\n");
    }
    s.push_str("\r\n");
    if out.write_all(s.as_bytes()).is_err() {
        return (false, false, String::new());
    }
    let mut hasher = sha2::Sha256::new();
    let mut whole = true;
    let mut sent = true;
    if body {
        let mut buf = vec![0u8; 32 << 10];
        loop {
            match response.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf.get(..n).unwrap_or_default();
                    hasher.update(data);
                    let wrote = if chunked {
                        write!(out, "{:x}\r\n", n)
                            .and_then(|()| out.write_all(data))
                            .and_then(|()| out.write_all(b"\r\n"))
                    } else {
                        out.write_all(data)
                    };
                    if wrote.is_err() {
                        sent = false;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    whole = false;
                    break;
                }
            }
        }
        if chunked && whole && sent {
            sent = out.write_all(b"0\r\n\r\n").is_ok();
        }
    }
    sent &= out.flush().is_ok();
    let digest = digest_of(hasher);
    (sent && whole && !close, whole && sent, digest)
}

/// How long a request's head may take (BuildKit's `ReadHeaderTimeout`).
const HEAD_TIME: Duration = Duration::from_secs(30);

/// The most connections a step holds open at once; more wait in the listener's backlog.
const CONNECTIONS: usize = 512;

/// The build's proxy: where it listens, its CA, and what its requests upstream trust.
pub struct Proxy {
    dir: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
    authority: ca::Authority,
    pub(super) tls: Arc<rustls::ClientConfig>,
    /// The addresses its requests may go to ([`reachable`]).
    pub(super) reach: fn(std::net::IpAddr) -> bool,
    /// How long a request's head may take ([`HEAD_TIME`]).
    pub(super) head_time: Duration,
    /// The most connections a step holds open at once ([`CONNECTIONS`]).
    pub(super) connections: usize,
    /// The room its files of request bodies have, past which they stop (SHARDS_KEEP_FREE).
    room: shards_image::store::Room,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Proxy {
    /// A proxy listening in `dir`, a directory of its own (mode 0700), with a CA of its
    /// own; its files of request bodies leave the disk what `limits` keeps free.
    pub fn new(dir: PathBuf, limits: &shards_image::store::Limits) -> Result<Proxy, String> {
        let socket = dir.join("proxy");
        let listener = UnixListener::bind(&socket).map_err(|e| format!("the build's proxy: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("the build's proxy: {e}"))?;
        let tls = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let room =
            shards_image::store::Room::new(&dir, limits).map_err(|e| format!("the build's proxy: {e}"))?;
        Ok(Proxy {
            dir,
            socket,
            listener,
            authority: ca::Authority::new()?,
            tls,
            reach: reachable,
            head_time: HEAD_TIME,
            connections: CONNECTIONS,
            room,
        })
    }

    /// The socket the builder's network process carries the proxy's flow to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The CA a step's trust bundle is given, PEM.
    pub fn ca(&self) -> &[u8] {
        self.authority.pem()
    }
}

/// A question a step's request puts to the build's policies: whether `method` may go to
/// `url` (redacted, as BuildKit's handler asks it).
#[derive(Debug)]
pub struct Ask {
    pub method: String,
    pub url: String,
    reply: mpsc::SyncSender<bool>,
}

impl Ask {
    pub fn answer(self, allowed: bool) {
        let _ = self.reply.send(allowed);
    }
}

/// One step's turn at the proxy: its connections, its requests' checks, what they come to.
pub struct Session<'p> {
    proxy: &'p Proxy,
    client: Client,
    cancel: Cancel,
    asks: mpsc::Sender<Ask>,
    /// Written to as a question is put, to wake the build's thread, which polls the other end.
    wake: UnixStream,
    capture: Mutex<Capture>,
    /// The step's connections, to shut as it ends, and the next one's number.
    live: Mutex<(u64, HashMap<u64, UnixStream>)>,
    ended: AtomicBool,
}

impl std::fmt::Debug for Session<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

/// What the build's thread holds of a step's session: its questions, and the socket it is
/// woken on when one comes.
#[derive(Debug)]
pub struct Questions {
    pub asks: mpsc::Receiver<Ask>,
    pub woken: UnixStream,
}

impl Questions {
    /// Answers each question waiting, with `check`.
    pub fn answer(&self, check: &mut dyn FnMut(&str, &str) -> bool) {
        let mut sink = [0u8; 64];
        while matches!((&self.woken).read(&mut sink), Ok(n) if n > 0) {}
        while let Ok(ask) = self.asks.try_recv() {
            let allowed = check(&ask.method, &ask.url);
            ask.answer(allowed);
        }
    }
}

impl<'p> Session<'p> {
    /// A step's session, its requests going upstream through `upstream`, the proxies the
    /// build's environment names; the build's thread takes its questions.
    pub fn begin(
        proxy: &'p Proxy,
        upstream: shards_registry::proxy::Proxies,
    ) -> Result<(Session<'p>, Questions), String> {
        let (asks, asked) = mpsc::channel();
        let (wake, woken) = UnixStream::pair().map_err(|e| format!("the build's proxy: {e}"))?;
        for s in [&wake, &woken] {
            s.set_nonblocking(true)
                .map_err(|e| format!("the build's proxy: {e}"))?;
        }
        let cancel = Cancel::new();
        let tls = proxy.tls.clone();
        // As the client wrote it: its own User-Agent or none; waiting as long as the step
        // does, as Go's transport has no bound; never to the host itself.
        let client = Client::new(Box::new(move |_| Ok(tls.clone())), "")
            .with_patience(Patience {
                head: None,
                stall: None,
            })
            .reaching(proxy.reach)
            .with_proxies(upstream)
            .cancelled_by(cancel.clone());
        Ok((
            Session {
                proxy,
                client,
                cancel,
                asks,
                wake,
                capture: Mutex::new(Capture::default()),
                live: Mutex::new((0, HashMap::new())),
                ended: AtomicBool::new(false),
            },
            Questions { asks: asked, woken },
        ))
    }

    /// Serves the step's connections, each on a thread of `scope`, until `stop` ends
    /// (its other end closed).
    pub fn serve<'s>(&'s self, scope: &'s std::thread::Scope<'s, '_>, stop: &'s UnixStream) {
        use std::os::fd::AsRawFd as _;
        loop {
            let mut polled = [self.proxy.listener.as_raw_fd(), stop.as_raw_fd()].map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            // SAFETY: poll(2) on two descriptors of ours.
            if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if polled[1].revents != 0 || self.ended.load(Ordering::SeqCst) {
                return;
            }
            if self.live.lock().unwrap_or_else(PoisonError::into_inner).1.len() >= self.proxy.connections {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let Ok((conn, _)) = self.proxy.listener.accept() else {
                continue;
            };
            let Some(id) = self.hold(&conn) else {
                continue;
            };
            let spawned = std::thread::Builder::new()
                .name("build proxy".into())
                .spawn_scoped(scope, move || {
                    let _ = conn.set_nonblocking(false);
                    self.connection(&conn);
                    self.let_go(id);
                });
            if spawned.is_err() {
                self.let_go(id);
            }
        }
    }

    /// Keeps a connection to shut as the step ends: none once it has.
    fn hold(&self, conn: &UnixStream) -> Option<u64> {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if self.ended.load(Ordering::SeqCst) {
            let _ = conn.shutdown(std::net::Shutdown::Both);
            return None;
        }
        let copy = conn.try_clone().ok()?;
        let id = live.0;
        live.0 += 1;
        live.1.insert(id, copy);
        Some(id)
    }

    fn let_go(&self, id: u64) {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .remove(&id);
    }

    /// The step has ended: each of its connections is shut and each request upstream
    /// cancelled, so every thread serving it ends.
    pub fn end(&self) {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        self.ended.store(true, Ordering::SeqCst);
        for c in live.1.values() {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        drop(live);
        self.cancel.cancel();
    }

    /// What the step's requests came to.
    pub fn capture(&self) -> Capture {
        self.capture
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the build's policies let `method` go to `url`: asked of the build's thread,
    /// and refused where it no longer answers.
    fn ask(&self, method: &str, url: &str) -> bool {
        let (reply, answer) = mpsc::sync_channel(1);
        let asked = capture::redact(url);
        let ask = Ask {
            method: method.to_string(),
            url: asked.clone(),
            reply,
        };
        if self.asks.send(ask).is_err() {
            return false;
        }
        let _ = (&self.wake).write(&[1]);
        let allowed = answer.recv().unwrap_or(false);
        if allowed {
            self.record(|c| c.allowed.push((method.to_string(), asked)));
        }
        allowed
    }

    fn record(&self, f: impl FnOnce(&mut Capture)) {
        f(&mut self.capture.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// `recordRequest`.
    fn record_request(&self, method: &str, url: &str, status: u16, redirect: &str) {
        self.record(|c| {
            c.requests.push(capture::Request {
                method: method.to_string(),
                url: capture::capture_url(url),
                redirect: capture::capture_url(redirect),
                status,
            });
        });
    }

    /// `recordIncomplete`.
    fn record_incomplete(&self, method: &str, url: &str, reason: &'static str) {
        self.record(|c| {
            c.incomplete.push(capture::Incomplete {
                method: method.to_string(),
                url: capture::capture_url(url),
                reason,
            });
        });
    }

    /// `recordResponse`: a material, or why the response is none, or nothing (no 2xx).
    fn record_response(
        &self,
        method: &str,
        url: &str,
        ranged: bool,
        status: u16,
        whole: bool,
        digest: String,
    ) {
        match capture::incomplete_reason(method, ranged, status, whole) {
            Some(reason) => self.record_incomplete(method, url, reason),
            None if (200..300).contains(&status) => self.record(|c| {
                c.materials.push(capture::Material {
                    url: capture::capture_url(url),
                    digest,
                });
            }),
            None => {}
        }
    }

    /// A connection of the step's, as Go's server serves BuildKit's handler.
    fn connection(&self, sock: &UnixStream) {
        let began = Instant::now();
        let mut reader = BufReader::with_capacity(4096, Timed { sock, deadline: None });
        let mut after_post = false;
        let mut first = true;
        loop {
            // A request's head within 30 s: of the connection for its first, of its first
            // byte for each after, the wait between unbounded.
            if !first {
                reader.get_mut().deadline = None;
                if !matches!(reader.fill_buf(), Ok(b) if !b.is_empty()) {
                    return;
                }
            }
            reader.get_mut().deadline =
                Some(if first { began } else { Instant::now() } + self.proxy.head_time);
            first = false;
            let head = head::read(&mut reader, true, after_post);
            reader.get_mut().deadline = None;
            let head = match head {
                Ok(h) => h,
                Err(fault) => {
                    if let Some(answer) = fault.answer() {
                        let _ = (&*sock).write_all(&answer);
                        let _ = sock.shutdown(std::net::Shutdown::Write);
                    }
                    return;
                }
            };
            after_post = head.method == "POST";
            if head.method == "CONNECT" {
                self.tunnel(&head, reader.buffer().to_vec(), sock);
                return;
            }
            if !self.request(&head, &mut reader, sock) {
                return;
            }
        }
    }

    /// One plain request (BuildKit's `ServeHTTP` for all but CONNECT): whether the
    /// connection goes on.
    fn request(&self, head: &Head, reader: &mut BufReader<Timed<'_>>, sock: &UnixStream) -> bool {
        let mut out = io::BufWriter::new(sock);
        let framing = head.framing().unwrap_or(Framing::Length(0));
        let has_request_body = framing != Framing::Length(0);
        let continues = head.has_token("Expect", "100-continue");
        // An expectation other than 100-continue is refused (`sendExpectationFailed`).
        if !continues && head.get("Expect").is_some_and(|e| !e.is_empty()) {
            let status = if head.at_least(1, 1) {
                "HTTP/1.1"
            } else {
                "HTTP/1.0"
            };
            let _ = write!(
                out,
                "{status} 417 Expectation Failed\r\nConnection: close\r\nDate: {}\r\nContent-Length: 0\r\n\r\n",
                http_date()
            );
            let _ = out.flush();
            return false;
        }
        if head.method == "OPTIONS" && head.target == "*" {
            return options(&mut out, head, reader, framing, continues);
        }
        let url = match request_url(head, None) {
            Ok(u) => u,
            Err(_) => return false,
        };
        let shown = String::from_utf8_lossy(&url.string()).into_owned();
        if !self.ask(&head.method, &shown) {
            // Refused, its body unread: where the connection would be kept, what is left
            // is dropped, as Go's server drops up to 256 KiB, or the connection closes (a
            // body past that, or one a client waits to be asked for).
            let forced = has_request_body
                && !keep_alive(head, true, false).0
                && ((continues && head.at_least(1, 1)) || !discard(reader, framing));
            return write_error(&mut out, head, 403, "Forbidden", forced);
        }
        // The body, read whole first: `100 Continue` where it waits for one.
        if continues && head.at_least(1, 1) && has_request_body {
            let _ = out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = out.flush();
        }
        let body = match self.spool(reader, framing) {
            Ok(b) => b,
            Err(Spooled::NoRoom) => {
                write_error(
                    &mut out,
                    head,
                    413,
                    "request body larger than the build's host has room for (SHARDS_KEEP_FREE)",
                    true,
                );
                return false;
            }
            // As Go's transport fails to send a body it cannot read: a 502, and the request
            // recorded as an upstream error.
            Err(Spooled::Failed(e)) => {
                self.record_request(&head.method, &shown, 502, "");
                self.record_incomplete(&head.method, &shown, "upstream_error");
                write_error(&mut out, head, 502, &e, true);
                return false;
            }
        };
        let ranged = head.get("Range").is_some_and(|r| !r.is_empty());
        let mut response = match self.upstream(head, &url, &body) {
            Ok(r) => r,
            Err(e) => {
                self.record_request(&head.method, &shown, 502, "");
                self.record_incomplete(&head.method, &shown, "upstream_error");
                return write_error(&mut out, head, 502, &e, false);
            }
        };
        let location = response.header("Location").map(str::to_string);
        self.record_request(
            &head.method,
            &shown,
            response.status,
            &capture::final_url(&url, location.as_deref()),
        );
        let (keep, whole, digest) = pass_on(&mut out, head, &mut response);
        self.record_response(&head.method, &shown, ranged, response.status, whole, digest);
        keep
    }

    /// A CONNECT (BuildKit's `handleConnect`): `200 Connection Established`, then TLS as
    /// the tunnel's host, each request in it checked and passed on, until one closes it.
    fn tunnel(&self, connect: &Head, buffered: Vec<u8>, sock: &UnixStream) {
        if (&*sock)
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .is_err()
        {
            return;
        }
        let authority = connect_host(connect);
        let Ok(config) = self.proxy.authority.config_for(strip_port(&authority)) else {
            return;
        };
        let Ok(conn) = rustls::ServerConnection::new(config) else {
            return;
        };
        let mut tls = rustls::StreamOwned::new(
            conn,
            Prefixed {
                pre: buffered,
                at: 0,
                sock,
            },
        );
        while tls.conn.is_handshaking() {
            if tls.conn.complete_io(&mut tls.sock).is_err() {
                return;
            }
        }
        let mut reader = BufReader::with_capacity(4096, tls);
        while let Ok(head) = head::read(&mut reader, false, false) {
            let url = match request_url(&head, Some(&authority)) {
                Ok(u) => u,
                Err(_) => break,
            };
            let shown = String::from_utf8_lossy(&url.string()).into_owned();
            if !self.ask(&head.method, &shown) {
                let _ = reader.get_mut().write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\nConnection: close\r\n\r\nForbidden\n",
                );
                let _ = reader.get_mut().flush();
                break;
            }
            let framing = head.framing().unwrap_or(Framing::Length(0));
            let body = match self.spool(&mut reader, framing) {
                Ok(b) => b,
                Err(Spooled::NoRoom) => {
                    let text = "request body larger than the build's host has room for (SHARDS_KEEP_FREE)\n";
                    let _ = write!(
                        reader.get_mut(),
                        "HTTP/1.1 413 Request Entity Too Large\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = reader.get_mut().flush();
                    break;
                }
                Err(Spooled::Failed(e)) => {
                    self.record_request(&head.method, &shown, 502, "");
                    self.record_incomplete(&head.method, &shown, "upstream_error");
                    let _ = write!(
                        reader.get_mut(),
                        "HTTP/1.1 502 Bad Gateway\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{e}\n",
                        e.len() + 1
                    );
                    let _ = reader.get_mut().flush();
                    break;
                }
            };
            let ranged = head.get("Range").is_some_and(|r| !r.is_empty());
            let mut response = match self.upstream(&head, &url, &body) {
                Ok(r) => r,
                Err(e) => {
                    self.record_request(&head.method, &shown, 502, "");
                    self.record_incomplete(&head.method, &shown, "upstream_error");
                    let _ = write!(
                        reader.get_mut(),
                        "HTTP/1.1 502 Bad Gateway\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{e}\n",
                        e.len() + 1
                    );
                    let _ = reader.get_mut().flush();
                    break;
                }
            };
            let location = response.header("Location").map(str::to_string);
            self.record_request(
                &head.method,
                &shown,
                response.status,
                &capture::final_url(&url, location.as_deref()),
            );
            let (keep, whole, digest) = write_through_tunnel(reader.get_mut(), &head, &mut response);
            self.record_response(&head.method, &shown, ranged, response.status, whole, digest);
            if !keep {
                break;
            }
        }
        // However the tunnel ends, its TLS ends with a close_notify, as Go's tls.Conn.Close
        // ends BuildKit's.
        reader.get_mut().conn.send_close_notify();
        let _ = reader.get_mut().flush();
    }

    /// Reads a request's body whole, in memory or, past [`IN_MEMORY`], a file of the
    /// proxy's, within the room its disk has (`Room`).
    fn spool<R: BufRead>(&self, reader: &mut R, framing: Framing) -> Result<Spool, Spooled> {
        let mut body = head::Body::new(reader, framing);
        // In memory until it outgrows it; then a file, removed as it drops, however this ends.
        let mut spool = Spool::Memory(Vec::new());
        let mut buf = vec![0u8; 32 << 10];
        loop {
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Spooled::Failed(format!("reading the request's body: {e}"))),
            };
            let chunk = buf.get(..n).unwrap_or_default();
            if let Spool::Memory(memory) = &mut spool {
                if memory.len() + n <= IN_MEMORY {
                    memory.extend_from_slice(chunk);
                    continue;
                }
                self.proxy.room.wrote(memory.len()).map_err(|_| Spooled::NoRoom)?;
                let (mut file, path) =
                    spool_file(&self.proxy.dir).map_err(|e| Spooled::Failed(e.to_string()))?;
                let held = file.write_all(memory);
                let size = memory.len() as u64;
                spool = Spool::File { file, path, size };
                held.map_err(|e| Spooled::Failed(e.to_string()))?;
            }
            if let Spool::File { file, size, .. } = &mut spool {
                self.proxy.room.wrote(n).map_err(|_| Spooled::NoRoom)?;
                file.write_all(chunk)
                    .map_err(|e| Spooled::Failed(e.to_string()))?;
                *size += n as u64;
            }
        }
        Ok(spool)
    }

    /// `roundTrip`: the request sent on as the client asked it, its URL's own target.
    fn upstream(
        &self,
        head: &Head,
        url: &gourl::Url,
        body: &Spool,
    ) -> Result<shards_registry::http::Response<'_>, String> {
        let scheme = match url.scheme.as_slice() {
            b"http" => Scheme::Http,
            b"https" => Scheme::Https,
            other => {
                return Err(format!(
                    "unsupported protocol scheme {}",
                    shards_cmdline::go::quote(&String::from_utf8_lossy(other))
                ));
            }
        };
        let (host, port) = url.host_port();
        let host = String::from_utf8_lossy(host).into_owned();
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let port = match port {
            b"" => None,
            p => Some(
                String::from_utf8_lossy(p)
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port {:?}", String::from_utf8_lossy(p)))?,
            ),
        };
        let target = String::from_utf8_lossy(&url.request_uri()).into_owned();
        let to = Url::request(scheme, &host, port, &target).map_err(|e| e.to_string())?;
        let headers = head.forwarded();
        let (bytes, file): (&[u8], _) = match body {
            Spool::Memory(m) => (m, None),
            Spool::File { file, size, .. } => (&[], Some((file, 0, *size))),
        };
        self.client
            .send(&Request {
                method: &head.method,
                url: &to,
                headers: &headers,
                body: bytes,
                file,
            })
            .map_err(|e| e.to_string())
    }
}

/// A step's connection, its reads bounded by a deadline where one is set.
struct Timed<'s> {
    sock: &'s UnixStream,
    deadline: Option<Instant>,
}

impl Read for Timed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = match self.deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the request's head took too long",
                    ));
                }
                Some(left)
            }
            None => None,
        };
        self.sock.set_read_timeout(left)?;
        (&*self.sock).read(buf)
    }
}

/// A tunnel's connection: what was read of it before the tunnel began, then the rest.
struct Prefixed<'s> {
    pre: Vec<u8>,
    at: usize,
    sock: &'s UnixStream,
}

impl Read for Prefixed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(rest) = self.pre.get(self.at..).filter(|r| !r.is_empty()) {
            let n = rest.len().min(buf.len());
            buf.get_mut(..n)
                .unwrap_or_default()
                .copy_from_slice(rest.get(..n).unwrap_or_default());
            self.at += n;
            return Ok(n);
        }
        let _ = self.sock.set_read_timeout(None);
        (&*self.sock).read(buf)
    }
}

impl Write for Prefixed<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.sock).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
