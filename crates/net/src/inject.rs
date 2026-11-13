//! The credential injector (D242; kiln plan 01a1d2b88be57304b972761e2a5c6781 step 3). A flow
//! whose TLS names a host a credential grant names has the guest's TLS ended here, under a leaf
//! the run's CA mints for that name as its ClientHello comes (as mitmproxy and gondolin mint
//! theirs, f34), and the host's made from here and verified as the registry's client verifies
//! (f20); both are TLS 1.3 under TLS_AES_256_GCM_SHA384 alone (f21). Between them the injector
//! is an HTTP/1.1 gateway (RFC 9110 §3.7): each request's head is read whole, its credential
//! fields' placeholders filled in where [`credential::decide`] says (f56, f65), a request for
//! another host refused 421 (RFC 9110 §7.4, f44), and its body passed on as it comes, never
//! read for a placeholder (f27); each response comes back as it came, but for the values filled
//! into its request, masked where they echo (f16). A flow to any other host passes as it came,
//! its ClientHello's bytes too, and carries no credential (f32, f33).
//!
//! It holds no socket and reads no clock: its connection (tcp.rs) gives it the guest's bytes
//! and the host's, and it puts what each side is to have in that connection's queues, whose
//! window bounds what it holds of the guest's. A value comes from the run's leases (lease.rs),
//! asked as a request first needs it; a request whose lease is asked is parked, and the lease's
//! answer resumes its flow.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::ops::Range;
use std::sync::{Arc, LazyLock};
use std::time::{Instant, SystemTime};

use rustls::client::Resumption;
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{Acceptor, NoServerSessionStorage};
use rustls::{ClientConfig, ClientConnection, ConnectionCommon, ServerConfig, ServerConnection};
use zeroize::Zeroizing;

use crate::Credentials;
use crate::dns::credential::{self, Fill};
use crate::lease::{self, Asked};
use crate::ring::Ring;

/// Each side's TLS (D242): TLS 1.3 under TLS13_AES_256_GCM_SHA384 alone, both ways (RFC 8446
/// §4.1.3, §7.3: one suite, each direction keyed apart), as D239 holds shards' links, and every
/// provider host a harness reaches takes it (f21). Its groups are aws-lc-rs's defaults with
/// post-quantum preferred, written out so that no crate's features reorder them: X25519MLKEM768,
/// which those hosts negotiated (f21), then X25519, secp256r1 and secp384r1, so that a guest's
/// TLS stack that offers no hybrid group has one in common (RFC 8446 §9.2: secp256r1 MUST be
/// implemented). D239's own provider is a type of hyper-quic's rustls fork (`rustls = { package
/// = "hyper-tls" }`), which crates.io's rustls, the verifier's, cannot take; the in-VM server
/// narrows aws-lc-rs's in the same way (crates/server).
static PROVIDER: LazyLock<Arc<CryptoProvider>> = LazyLock::new(|| {
    Arc::new(CryptoProvider {
        cipher_suites: vec![aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384],
        kx_groups: vec![
            aws_lc_rs::kx_group::X25519MLKEM768,
            aws_lc_rs::kx_group::X25519,
            aws_lc_rs::kx_group::SECP256R1,
            aws_lc_rs::kx_group::SECP384R1,
        ],
        ..aws_lc_rs::default_provider()
    })
});

/// The one application protocol each side offers (RFC 7301): the injector speaks HTTP/1.1
/// alone, so a guest offering only another is refused at its handshake.
const HTTP_1_1: &[u8] = b"http/1.1";

/// The host's side of an injected flow's TLS (D242): verified as the registry's client
/// verifies (D19, f20), offering HTTP/1.1 alone, and resuming no session, as a session kept is
/// a cache nothing measured (R05). Made as the network process starts, before it confines
/// itself, as on Linux the verifier reads the system's roots then (shards-verify).
#[derive(Debug, Clone)]
pub struct Upstream(Arc<ClientConfig>);

impl Upstream {
    /// Trusting the host's roots, and `extra_roots` (a test's).
    pub fn new(extra_roots: Vec<CertificateDer<'static>>) -> Result<Upstream, String> {
        let provider = PROVIDER.clone();
        let verifier = shards_verify::verifier(extra_roots, provider.clone())
            .map_err(|e| format!("the injector's verifier: {e}"))?;
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| format!("the injector's TLS: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = vec![HTTP_1_1.to_vec()];
        config.resumption = Resumption::disabled();
        Ok(Upstream(Arc::new(config)))
    }
}

/// The guest's side for a flow to `name`'s host: the leaf the run's CA mints for it, HTTP/1.1
/// alone, no session kept and no ticket sent (R05).
fn guest_config(cert: Vec<u8>, key: Vec<u8>) -> Result<Arc<ServerConfig>, rustls::Error> {
    let mut config = ServerConfig::builder_with_provider(PROVIDER.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )?;
    config.alpn_protocols = vec![HTTP_1_1.to_vec()];
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(Arc::new(config))
}

/// What a flow reaches of its run as it is served (D242): the run's credentials, the token its
/// leases resume it by, the time it is served at, and what it filled in, for the audit.
#[derive(Debug)]
pub struct Run<'a> {
    pub credentials: &'a mut Credentials,
    pub waiter: lease::Waiter,
    pub now: Instant,
    pub audit: Vec<Audit>,
}

/// A value filled in (D242, f26): toward which host, which credential, and when; never the
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub host: String,
    pub reference: String,
    pub at: SystemTime,
}

impl fmt::Display for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (reference, host) = (&self.reference, &self.host);
        match self.at.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(t) => write!(
                f,
                "shards-net: credential {reference} filled in toward {host} at {}.{:09} (Unix time)",
                t.as_secs(),
                t.subsec_nanos()
            ),
            Err(_) => write!(
                f,
                "shards-net: credential {reference} filled in toward {host}, the clock before the Unix epoch"
            ),
        }
    }
}

/// Where a flow's bytes go: its connection's queues, to the host's socket and to the guest.
#[derive(Debug)]
pub struct Queues<'a> {
    pub to_host: &'a mut Ring,
    pub to_guest: &'a mut Ring,
}

/// A queue, written to.
struct Onto<'a>(&'a mut Ring);

impl Write for Onto<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What `tls` has to send, onto `ring`.
fn send<D>(tls: &mut ConnectionCommon<D>, ring: &mut Ring) -> io::Result<()> {
    while tls.wants_write() {
        if tls.write_tls(&mut Onto(ring))? == 0 {
            break;
        }
    }
    Ok(())
}

/// What `tls` has decrypted, onto `into`; whether its peer has ended its side, by its
/// close_notify or its connection's end.
fn plain<D>(tls: &mut ConnectionCommon<D>, into: &mut Vec<u8>) -> bool {
    loop {
        let mut reader = tls.reader();
        match reader.fill_buf() {
            Ok([]) => return true,
            Ok(chunk) => {
                let n = chunk.len();
                into.extend_from_slice(chunk);
                reader.consume(n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

/// What a flow asks of its connection once it has taken what it was given.
#[derive(Debug, PartialEq, Eq)]
pub enum Said {
    /// Nothing but what its queues hold.
    Going,
    /// The flow is no granted host's: these guest bytes go to the host as they came, and from
    /// here its connection carries its bytes as any other's, the flow let go of. Said as host
    /// bytes come, those bytes go to the guest as they came.
    Pass(Vec<u8>),
    /// Its last bytes are queued: the guest's side ends once they have gone, and the host's.
    Close,
    /// It cannot go on: the connection is reset.
    Reset,
}

/// One flow's injector, from the guest's first byte (R04: its states).
pub struct Flow {
    state: State,
}

impl fmt::Debug for Flow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.state {
            State::Hello { .. } => "Flow(Hello)",
            State::Ended(_) => "Flow(Ended)",
            State::Gone => "Flow(Gone)",
        })
    }
}

enum State {
    /// The guest's first bytes, until its ClientHello is whole: kept as they came, so that a
    /// flow to a host no grant names gets them unread. rustls holds a ClientHello to its own
    /// bound (64 KiB, `MAX_HANDSHAKE_SIZE`), which RFC 8446 §4 lets a receiver set below its
    /// 2^24 bytes; one past it is no ClientHello this side reads, and passes.
    Hello { acceptor: Box<Acceptor>, seen: Vec<u8> },
    /// A granted host's: the guest's TLS ended here, the host's made from here.
    Ended(Box<Ended>),
    /// Let go of, its connection carrying the rest.
    Gone,
}

impl Default for Flow {
    fn default() -> Flow {
        Flow::new()
    }
}

impl Flow {
    pub fn new() -> Flow {
        Flow {
            state: State::Hello {
                acceptor: Box::default(),
                seen: Vec::new(),
            },
        }
    }

    /// What the flow holds of the guest's bytes and has not passed on: a part of its
    /// connection's window, as the guest sent them while it waited.
    pub fn held(&self) -> usize {
        match &self.state {
            State::Hello { seen, .. } => seen.len(),
            State::Ended(e) => e.held(),
            State::Gone => 0,
        }
    }

    /// The guest's bytes, in order.
    pub fn from_guest(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.hello(bytes, run, q),
            State::Ended(e) => e.from_guest(bytes, run, q),
            State::Gone => Said::Going,
        }
    }

    /// The host's bytes, in order. Before the guest's ClientHello is whole, the host spoke
    /// first, which no TLS server does: the flow passes, these bytes to the guest as they came.
    pub fn from_host(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.pass(),
            State::Ended(e) => e.from_host(bytes, run, q),
            State::Gone => Said::Going,
        }
    }

    /// The guest has ended its side (its FIN).
    pub fn guest_ended(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.pass(),
            State::Ended(e) => e.guest_ended(run, q),
            State::Gone => Said::Going,
        }
    }

    /// The host has ended its side (its socket's end).
    pub fn host_ended(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.pass(),
            State::Ended(e) => e.host_ended(run, q),
            State::Gone => Said::Going,
        }
    }

    /// A lease the flow waited on was answered: its request goes on, or is refused.
    pub fn resume(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Ended(e) => e.serve(run, q),
            State::Hello { .. } | State::Gone => Said::Going,
        }
    }

    /// Lets the flow go, the guest's bytes it held to pass on as they came.
    fn pass(&mut self) -> Said {
        match std::mem::replace(&mut self.state, State::Gone) {
            State::Hello { seen, .. } => Said::Pass(seen),
            other => {
                self.state = other;
                Said::Going
            }
        }
    }

    /// The guest's first bytes, read for its ClientHello: a flow whose server name a grant
    /// names has its TLS ended here; any other, or one whose bytes are no TLS this side reads,
    /// passes as it came.
    fn hello(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        let State::Hello { acceptor, seen } = &mut self.state else {
            return Said::Going;
        };
        seen.extend_from_slice(bytes);
        let mut rest = bytes;
        let accepted = loop {
            if !rest.is_empty() && !matches!(acceptor.read_tls(&mut rest), Ok(n) if n > 0) {
                return self.pass();
            }
            match acceptor.accept() {
                Ok(Some(accepted)) => break accepted,
                Ok(None) if rest.is_empty() => return Said::Going,
                Ok(None) => {}
                Err(_) => return self.pass(),
            }
        };
        let Some(sni) = accepted.client_hello().server_name().map(str::to_ascii_lowercase) else {
            return self.pass();
        };
        let granted = run
            .credentials
            .grants
            .iter()
            .any(|g| g.toward.iter().any(|h| h.eq_ignore_ascii_case(&sni)));
        if !granted {
            return self.pass();
        }
        let ended = match Ended::new(&sni, accepted, rest, run, q) {
            Ok(ended) => ended,
            Err(said) => {
                self.state = State::Gone;
                return said;
            }
        };
        self.state = State::Ended(Box::new(ended));
        let State::Ended(e) = &mut self.state else {
            return Said::Reset;
        };
        e.serve(run, q)
    }
}

/// Says on stderr (the run host's diagnostics) why a flow to `sni` ended: never a value, as
/// TLS's and HTTP's errors hold none.
fn say(sni: &str, what: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "shards-net: {sni}: {what}");
}

/// Why the injector answers a request itself (R04): its status (RFC 9110 §15) and its cause;
/// the connection ends after it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    /// A head this side cannot read as one, or framing it will not pass on (RFC 9112 §6.3,
    /// f43): 400.
    Malformed(&'static str),
    /// A head past what its connection holds of the guest's bytes: 431 (RFC 6585 §5).
    TooLarge,
    /// The request's host is not the name its connection's TLS was ended under: 421 (RFC 9110
    /// §7.4, f44).
    Misdirected { sni: String, host: String },
    /// A credential it carries was not leased, and why: 503.
    Unleased { reference: String, why: lease::Refusal },
    /// The host's TLS failed, or it ended before it answered: 502.
    Upstream(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Malformed(why) => write!(f, "the request was refused: {why}"),
            Refusal::TooLarge => f.write_str("the request's head is past what its connection holds"),
            Refusal::Misdirected { sni, host } => write!(
                f,
                "this connection's TLS is {sni}'s, and the request is {host}'s (RFC 9110 §7.4)"
            ),
            Refusal::Unleased { reference, why } => write!(f, "credential {reference} was not leased: {why}"),
            Refusal::Upstream(why) => write!(f, "the host's side failed: {why}"),
        }
    }
}

impl Refusal {
    /// Its status and reason phrase (RFC 9110 §15; RFC 6585 §5 for 431).
    fn status(&self) -> (u16, &'static str) {
        match self {
            Refusal::Malformed(_) => (400, "Bad Request"),
            Refusal::TooLarge => (431, "Request Header Fields Too Large"),
            Refusal::Misdirected { .. } => (421, "Misdirected Request"),
            Refusal::Unleased { .. } => (503, "Service Unavailable"),
            Refusal::Upstream(_) => (502, "Bad Gateway"),
        }
    }

    /// The response the guest gets, the connection closing after it.
    fn response(&self) -> Vec<u8> {
        let (status, reason) = self.status();
        let body = format!("shards: {self}\n");
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

/// What the host owes for the request it was last given, until its final response ends.
#[derive(Debug, Clone, Copy, Default)]
struct Owed {
    /// The request was HEAD's: its response has no body (RFC 9112 §6.3 rule 1).
    head: bool,
    /// CONNECT's: a 2xx makes the connection a tunnel (rule 2).
    connect: bool,
    /// It asked an upgrade (RFC 9110 §7.8): a 101 makes the connection a tunnel.
    upgrade: bool,
}

/// Where the guest's requests stand (R04).
enum Request {
    /// A request's head coming, in `pending`.
    Head,
    /// A head read whole, waiting for the host's handshake or its values' leases.
    Parked(Box<Parked>),
    /// Its body going to the host as it comes, as framed.
    Body(Framing),
    /// Sent whole: the next head waits for its response's end, one request at a time.
    Sent,
    /// Upgraded or tunnelled: the guest's bytes go to the host as they come.
    Tunnel,
    /// Refused, or the flow ending: nothing more is taken.
    Done,
}

/// A request read whole and waiting to go: its head, what of it goes (the hop-by-hop fields
/// cut, each placeholder's place and its credential), its host, its body's framing, and what
/// its response will be.
struct Parked {
    head: Vec<u8>,
    edits: Vec<(Range<usize>, Vec<u8>)>,
    fills: Vec<(Range<usize>, String)>,
    host: String,
    body: Framing,
    owed: Owed,
}

/// Where the host's responses stand (R04).
enum Response {
    /// A response's head coming.
    Head(Vec<u8>),
    /// Its body coming to the guest as it comes, as framed.
    Body(Framing),
    /// Upgraded or tunnelled: the host's bytes go to the guest as they come.
    Tunnel,
}

/// A message body's framing (RFC 9112 §6.3), where in it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Framing {
    /// This many bytes left (rule 6).
    Length(u64),
    /// Chunked (§7.1).
    Chunked(Chunk),
    /// Until the host ends (rule 8; a response's alone).
    Close,
}

/// Where in a chunked body (RFC 9112 §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chunk {
    /// A chunk's size line, as it has come.
    Size(Vec<u8>),
    /// This many bytes of a chunk's data left.
    Data(u64),
    /// The CRLF after a chunk's data: how much of it has come.
    DataEnd(usize),
    /// The trailer section after the last chunk (§7.1.2), as it has come.
    Trailers(Vec<u8>),
}

/// A body's bytes, as its framing reads them: data, or the framing between.
enum Piece<'b> {
    Data(&'b [u8]),
    Frame(&'b [u8]),
}

/// A body's framing broken: no message this side passes on (RFC 9112 §6.3, f43).
#[derive(Debug)]
struct Broken(&'static str);

/// Walks `input` through `framing`, giving each piece to `out`: how many of its bytes the body
/// took, all of them unless it ended, and whether it ended. A size line or a trailer section
/// past `most` is broken: it would be held past what the connection holds.
fn walk(
    framing: &mut Framing,
    input: &[u8],
    most: usize,
    out: &mut dyn FnMut(Piece<'_>) -> io::Result<()>,
) -> Result<(usize, bool), Broken> {
    let mut at = 0usize;
    let fail = |_: io::Error| Broken("its bytes could not be queued");
    loop {
        let rest = input.get(at..).unwrap_or_default();
        match framing {
            Framing::Length(0) => return Ok((at, true)),
            Framing::Length(left) => {
                let n = rest.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                if n == 0 {
                    return Ok((at, false));
                }
                out(Piece::Data(rest.get(..n).unwrap_or_default())).map_err(fail)?;
                *left -= n as u64;
                at += n;
            }
            Framing::Close => {
                if !rest.is_empty() {
                    out(Piece::Data(rest)).map_err(fail)?;
                }
                return Ok((input.len(), false));
            }
            Framing::Chunked(chunk) => {
                if rest.is_empty() {
                    return Ok((at, false));
                }
                match chunk {
                    Chunk::Size(line) => {
                        let n = rest
                            .iter()
                            .position(|&b| b == b'\n')
                            .map_or(rest.len(), |i| i + 1);
                        line.extend_from_slice(rest.get(..n).unwrap_or_default());
                        at += n;
                        if line.len() > most {
                            return Err(Broken("a chunk's size line past what the connection holds"));
                        }
                        if line.last() != Some(&b'\n') {
                            continue;
                        }
                        let size = chunk_size(line).ok_or(Broken("a chunk's size line"))?;
                        out(Piece::Frame(line)).map_err(fail)?;
                        *chunk = if size == 0 {
                            Chunk::Trailers(Vec::new())
                        } else {
                            Chunk::Data(size)
                        };
                    }
                    Chunk::Data(left) => {
                        let n = rest.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                        out(Piece::Data(rest.get(..n).unwrap_or_default())).map_err(fail)?;
                        *left -= n as u64;
                        at += n;
                        if *left == 0 {
                            *chunk = Chunk::DataEnd(0);
                        }
                    }
                    Chunk::DataEnd(seen) => {
                        let want = b"\r\n".get(*seen..).unwrap_or_default();
                        let n = want.len().min(rest.len());
                        if rest.get(..n) != want.get(..n) {
                            return Err(Broken("a chunk's data not ended by CRLF"));
                        }
                        out(Piece::Frame(rest.get(..n).unwrap_or_default())).map_err(fail)?;
                        at += n;
                        *seen += n;
                        if *seen == 2 {
                            *chunk = Chunk::Size(Vec::new());
                        }
                    }
                    Chunk::Trailers(section) => {
                        let n = rest
                            .iter()
                            .position(|&b| b == b'\n')
                            .map_or(rest.len(), |i| i + 1);
                        section.extend_from_slice(rest.get(..n).unwrap_or_default());
                        at += n;
                        if section.len() > most {
                            return Err(Broken("a trailer section past what the connection holds"));
                        }
                        if !lines_end_in_crlf(section) {
                            return Err(Broken("a trailer line ends in a bare LF or CR"));
                        }
                        if section.as_slice() == b"\r\n" || section.ends_with(b"\r\n\r\n") {
                            // The trailers are fields too, which an echo may lie in.
                            out(Piece::Data(section)).map_err(fail)?;
                            *framing = Framing::Length(0);
                            return Ok((at, true));
                        }
                    }
                }
            }
        }
    }
}

/// A chunk's size line (RFC 9112 §7.1: `chunk-size [ chunk-ext ] CRLF`, `chunk-size =
/// 1*HEXDIG`): its size; none for a line that is not one, a control byte in its extensions
/// (a lone LF there is where another reader could end the line) or a size past a u64.
fn chunk_size(line: &[u8]) -> Option<u64> {
    let line = line.strip_suffix(b"\r\n")?;
    let digits = line.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    let (hex, ext) = line.split_at_checked(digits)?;
    if hex.is_empty()
        || !(ext.is_empty() || ext.trim_ascii_start().first() == Some(&b';'))
        || ext.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f)
    {
        return None;
    }
    hex.iter().try_fold(0u64, |size, &b| {
        let digit = char::from(b).to_digit(16)?;
        size.checked_mul(16)?.checked_add(u64::from(digit))
    })
}

/// Whether every LF in `bytes` ends a CRLF and every CR starts one, a CR last awaiting its LF.
fn lines_end_in_crlf(bytes: &[u8]) -> bool {
    bytes.iter().enumerate().all(|(i, &b)| match b {
        b'\n' => i > 0 && bytes.get(i - 1) == Some(&b'\r'),
        b'\r' => bytes.get(i + 1).is_none_or(|&n| n == b'\n'),
        _ => true,
    })
}

/// Where a head ends, past its empty line; none while it has not come whole. A line ended by a
/// bare LF or CR is refused: RFC 9112 §2.2 lets a recipient take a lone LF, and a host that
/// takes it where this side does not would read another message than this side passed on.
fn head_end(buf: &[u8]) -> Result<Option<usize>, ()> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    let read = end.map_or(buf, |end| buf.get(..end).unwrap_or_default());
    if !lines_end_in_crlf(read) {
        return Err(());
    }
    Ok(end)
}

/// A head's field as it lies in the head: its name, lowered, and where its line and value are.
#[derive(Debug, Clone)]
struct Field {
    name: String,
    line: Range<usize>,
    value: Range<usize>,
}

/// Where `part`, a slice of `buf`, lies in it.
fn place(buf: &[u8], part: &[u8]) -> Option<Range<usize>> {
    let start = (part.as_ptr() as usize).checked_sub(buf.as_ptr() as usize)?;
    let end = start.checked_add(part.len())?;
    (end <= buf.len()).then_some(start..end)
}

/// The fields httparse read of `head`, where each lies: each line from its name to its CRLF.
fn fields(head: &[u8], read: &[httparse::Header<'_>]) -> Option<Vec<Field>> {
    read.iter()
        .map(|h| {
            let name = place(head, h.name.as_bytes())?;
            let value = place(head, h.value)?;
            let after = head.get(value.end..)?;
            let crlf = after.windows(2).position(|w| w == b"\r\n")?;
            Some(Field {
                name: h.name.to_ascii_lowercase(),
                line: name.start..value.end + crlf + 2,
                value,
            })
        })
        .collect()
}

/// Each field's value of `name` in `head`.
fn values<'h>(head: &'h [u8], fields: &'h [Field], name: &'h str) -> impl Iterator<Item = &'h [u8]> + 'h {
    fields
        .iter()
        .filter(move |f| f.name == name)
        .map(|f| head.get(f.value.clone()).unwrap_or_default())
}

/// A comma-separated list's elements (RFC 9110 §5.6.1), trimmed, the empty ones dropped.
fn list(value: &[u8]) -> impl Iterator<Item = &[u8]> {
    value
        .split(|&b| b == b',')
        .map(<[u8]>::trim_ascii)
        .filter(|e| !e.is_empty())
}

/// The fields a hop names that no other hop is to have (RFC 9110 §7.6.1): each field a
/// connection option names, and the option with it, but for `close`, `keep-alive` and
/// `upgrade`, which the injector applies to both hops alike, one connection each way for one,
/// so that they are its own options for the other hop as well; and `Proxy-Connection`, which a
/// gateway's hop has no use of. Each as an edit of `head`: a field's line taken out, or a
/// Connection field's value given the options it keeps.
fn hop_by_hop(head: &[u8], fields: &[Field]) -> Vec<(Range<usize>, Vec<u8>)> {
    let kept = |o: &[u8]| {
        o.eq_ignore_ascii_case(b"close")
            || o.eq_ignore_ascii_case(b"keep-alive")
            || o.eq_ignore_ascii_case(b"upgrade")
    };
    let named: Vec<String> = values(head, fields, "connection")
        .flat_map(list)
        .filter(|o| !kept(o))
        .map(|o| String::from_utf8_lossy(o).to_ascii_lowercase())
        .collect();
    let mut edits: Vec<(Range<usize>, Vec<u8>)> = fields
        .iter()
        .filter(|f| f.name == "proxy-connection" || named.contains(&f.name))
        .map(|f| (f.line.clone(), Vec::new()))
        .collect();
    if !named.is_empty() {
        for f in fields.iter().filter(|f| f.name == "connection") {
            let value = head.get(f.value.clone()).unwrap_or_default();
            let options: Vec<&[u8]> = list(value).filter(|o| kept(o)).collect();
            if options.is_empty() {
                edits.push((f.line.clone(), Vec::new()));
            } else {
                edits.push((f.value.clone(), options.join(&b", "[..])));
            }
        }
    }
    edits.sort_by_key(|(r, _)| r.start);
    edits
}

/// A field's value as a credential's token: an `Authorization` field's bearer token (RFC 6750
/// §2.1, its scheme of any case, RFC 9110 §11.1) or an `x-api-key` field's whole value, the
/// two fields the harnesses send theirs in (f56, f65), where each lies in the head.
fn token(head: &[u8], field: &Field) -> Option<Range<usize>> {
    let value = head.get(field.value.clone())?;
    match field.name.as_str() {
        "authorization" => {
            let space = value.iter().position(|&b| b == b' ')?;
            let (scheme, rest) = value.split_at_checked(space)?;
            if !scheme.eq_ignore_ascii_case(b"bearer") {
                return None;
            }
            let skipped = rest.iter().take_while(|&&b| b == b' ').count();
            let start = field.value.start + space + skipped;
            (start < field.value.end && !value.get(space + skipped..)?.contains(&b' '))
                .then_some(start..field.value.end)
        }
        "x-api-key" => (!value.is_empty()).then_some(field.value.clone()),
        _ => None,
    }
}

/// The host a request is for (RFC 9112 §3.2, §3.2.2): its target's authority where the target
/// is in absolute form, which an origin server takes over its `Host`; else its one `Host`'s.
/// Without its port, lowered, as `decide` compares hosts.
fn request_host(target: &str, head: &[u8], fields: &[Field]) -> Result<String, Refusal> {
    let mut hosts = values(head, fields, "host");
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return Err(Refusal::Malformed(
            "a request names its host once (RFC 9112 §3.2)",
        ));
    };
    let authority = match target.split_once("://") {
        Some((scheme, rest))
            if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http") =>
        {
            rest.split(['/', '?', '#']).next().unwrap_or_default().as_bytes()
        }
        _ => host,
    };
    let authority =
        std::str::from_utf8(authority).map_err(|_| Refusal::Malformed("a host that is not text"))?;
    // Without its user information, then its port, an IPv6 literal kept whole with its
    // brackets (RFC 3986 §3.2.2).
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let name = match authority.find(']') {
        Some(close) if authority.starts_with('[') => authority.get(..=close).unwrap_or(authority),
        _ => authority
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(authority, |(name, _)| name),
    };
    if name.is_empty() {
        return Err(Refusal::Malformed("a request's host is empty"));
    }
    Ok(name.to_ascii_lowercase())
}

/// A request's body's framing (RFC 9112 §6.3, f43): chunked where its Transfer-Encoding ends in
/// `chunked`, its Content-Length's where it has one, else none. Refused: Transfer-Encoding with
/// Content-Length beside it, which is request smuggling's shape (rule 3), a Transfer-Encoding
/// whose last coding is not `chunked` or that codes `chunked` twice (rule 4, §6.1), one in an
/// HTTP/1.0 request (§6.1), and a Content-Length that is not one length (rule 5).
fn request_framing(head: &[u8], fields: &[Field], minor: u8) -> Result<Framing, Refusal> {
    let codings: Vec<&[u8]> = values(head, fields, "transfer-encoding").flat_map(list).collect();
    let lengths: Vec<&[u8]> = values(head, fields, "content-length").flat_map(list).collect();
    if !codings.is_empty() {
        if !lengths.is_empty() {
            return Err(Refusal::Malformed(
                "a request framed by both Transfer-Encoding and Content-Length",
            ));
        }
        if minor == 0 {
            return Err(Refusal::Malformed("a transfer coding in an HTTP/1.0 request"));
        }
        let chunked = codings
            .iter()
            .filter(|c| c.eq_ignore_ascii_case(b"chunked"))
            .count();
        if chunked != 1 || !codings.last().is_some_and(|c| c.eq_ignore_ascii_case(b"chunked")) {
            return Err(Refusal::Malformed(
                "a request whose transfer coding does not end in chunked, once",
            ));
        }
        return Ok(Framing::Chunked(Chunk::Size(Vec::new())));
    }
    match length(&lengths) {
        Some(Ok(n)) => Ok(Framing::Length(n)),
        Some(Err(())) => Err(Refusal::Malformed("a Content-Length that is not one length")),
        None => Ok(Framing::Length(0)),
    }
}

/// A Content-Length's one length (RFC 9110 §8.6: its values all one decimal); none where there
/// is none.
fn length(lengths: &[&[u8]]) -> Option<Result<u64, ()>> {
    let first = lengths.first()?;
    let parse = |v: &[u8]| -> Option<u64> {
        (!v.is_empty() && v.iter().all(u8::is_ascii_digit))
            .then(|| std::str::from_utf8(v).ok()?.parse().ok())
            .flatten()
    };
    Some(match parse(first) {
        Some(n) if lengths.iter().all(|v| parse(v) == Some(n)) => Ok(n),
        _ => Err(()),
    })
}

/// A response's body's framing (RFC 9112 §6.3), to a request owed `owed`: none where its
/// request was HEAD's or its status is 1xx, 204 or 304 (rule 1); chunked where its
/// Transfer-Encoding ends in `chunked`, else until the host ends (rule 4); its Content-Length's
/// (rule 6), which is broken where it is not one length (rule 5); else until the host ends
/// (rule 8).
fn response_framing(status: u16, owed: Owed, head: &[u8], fields: &[Field]) -> Result<Framing, Broken> {
    if owed.head || (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok(Framing::Length(0));
    }
    let codings: Vec<&[u8]> = values(head, fields, "transfer-encoding").flat_map(list).collect();
    if let Some(last) = codings.last() {
        return Ok(if last.eq_ignore_ascii_case(b"chunked") {
            Framing::Chunked(Chunk::Size(Vec::new()))
        } else {
            Framing::Close
        });
    }
    let lengths: Vec<&[u8]> = values(head, fields, "content-length").flat_map(list).collect();
    match length(&lengths) {
        Some(Ok(n)) => Ok(Framing::Length(n)),
        Some(Err(())) => Err(Broken("a response's Content-Length that is not one length")),
        None => Ok(Framing::Close),
    }
}

/// `head` with each edit's range given its bytes (none, for a field taken out), in one buffer
/// made to its length at once, wiped as it goes: no buffer let go of as it grows holds a part
/// of a value (D242's wiping). Edits do not overlap: a value is filled into a credential
/// field's value, and a hop-by-hop edit takes a field no credential's token is read from, or
/// a Connection field's value.
fn rewrite(head: &[u8], edits: &[(Range<usize>, &[u8])]) -> Zeroizing<Vec<u8>> {
    let mut edits: Vec<&(Range<usize>, &[u8])> = edits.iter().collect();
    edits.sort_by_key(|(r, _)| r.start);
    let len = edits.iter().fold(head.len(), |len, (r, v)| {
        len.saturating_sub(r.len()).saturating_add(v.len())
    });
    let mut out = Zeroizing::new(Vec::with_capacity(len));
    let mut at = 0usize;
    for (r, v) in edits {
        if r.start < at {
            continue;
        }
        out.extend_from_slice(head.get(at..r.start).unwrap_or_default());
        out.extend_from_slice(v);
        at = r.end;
    }
    out.extend_from_slice(head.get(at..).unwrap_or_default());
    out
}

/// A value to mask in a response, and the placeholder its bytes are masked with.
struct Masked<'a> {
    value: &'a [u8],
    placeholder: &'a [u8],
    /// KMP's failure function of `value`.
    table: Vec<usize>,
}

/// KMP's failure function of `v` (Knuth, Morris and Pratt 1977): for each of its prefixes, the
/// length of its longest proper prefix that is also its suffix.
fn failure(v: &[u8]) -> Vec<usize> {
    let mut f = vec![0usize; v.len()];
    let mut k = 0usize;
    for i in 1..v.len() {
        let c = v.get(i);
        while k > 0 && v.get(k) != c {
            k = f.get(k - 1).copied().unwrap_or(0);
        }
        if v.get(k) == c {
            k += 1;
        }
        if let Some(slot) = f.get_mut(i) {
            *slot = k;
        }
    }
    f
}

/// How much of `v` data ends with once `b` follows data that ended with `m` of it.
fn step(v: &[u8], table: &[usize], mut m: usize, b: u8) -> usize {
    while m > 0 && v.get(m) != Some(&b) {
        m = table.get(m - 1).copied().unwrap_or(0);
    }
    if v.get(m) == Some(&b) { m + 1 } else { 0 }
}

/// A response's echoes of the values filled into its request, replaced as they come back (f16)
/// by as many bytes of the value's placeholder, cycled: the whole placeholder where the value is
/// as long or longer, a part of it where shorter. No length changes, so a response's framing
/// stays as it came and none is recomputed (RFC 9112 §6.3, f43). A value is sought in a
/// response's head and data alone, a chunked body's framing passed over, so that an echo split
/// across chunks is found. The bytes from where a value may still begin wait until it completes
/// or fails; the rest goes on at once, and a message's end lets everything go.
#[derive(Default)]
struct Mask {
    /// The credentials filled into the request under way, each with how much of its value the
    /// data so far ends with.
    filled: Vec<(String, usize)>,
    /// Bytes not yet let go of, from the stream's byte `base`.
    queue: Zeroizing<Vec<u8>>,
    base: usize,
    /// The stream's offsets of the last data bytes, as many as the longest value.
    at: VecDeque<usize>,
}

impl Mask {
    /// Each value filled into the request under way, and its placeholder, as its run holds
    /// them: a value whose lease has ended is not held, and nothing is masked of it.
    fn values<'r>(&self, run: &'r Run<'_>) -> Vec<Option<Masked<'r>>> {
        self.filled
            .iter()
            .map(|(reference, _)| {
                let value = run.credentials.leases.held(reference, run.now)?;
                let placeholder = run
                    .credentials
                    .grants
                    .iter()
                    .find(|g| &g.reference == reference)?
                    .placeholder
                    .as_bytes();
                Some(Masked {
                    value,
                    placeholder,
                    table: failure(value),
                })
            })
            .collect()
    }

    /// `bytes` of a response, data an echo may lie in or its framing, on their way to `out`.
    fn push(
        &mut self,
        bytes: &[u8],
        data: bool,
        run: &Run<'_>,
        out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.filled.is_empty() && self.queue.is_empty() {
            return out(bytes);
        }
        let values = self.values(run);
        let longest = values.iter().flatten().map(|m| m.value.len()).max().unwrap_or(0);
        if longest == 0 {
            // No value is held to seek: what waited for one goes on.
            for (_, m) in &mut self.filled {
                *m = 0;
            }
            self.at.clear();
        }
        for &b in bytes {
            let pos = self.base + self.queue.len();
            self.queue.push(b);
            if !data || longest == 0 {
                continue;
            }
            self.at.push_back(pos);
            while self.at.len() > longest {
                self.at.pop_front();
            }
            let mut whole = None;
            for ((_, m), masked) in self.filled.iter_mut().zip(&values) {
                let Some(v) = masked else {
                    *m = 0;
                    continue;
                };
                *m = step(v.value, &v.table, *m, b);
                if *m == v.value.len() {
                    whole = Some(v);
                }
            }
            if let Some(v) = whole {
                let len = v.value.len();
                let from = self.at.len().saturating_sub(len);
                for (i, &p) in self.at.iter().skip(from).enumerate() {
                    let mask = v
                        .placeholder
                        .get(i.checked_rem(v.placeholder.len()).unwrap_or(0))
                        .copied();
                    if let Some(byte) = self.queue.get_mut(p.wrapping_sub(self.base)) {
                        *byte = mask.unwrap_or(b'*');
                    }
                }
                for (_, m) in &mut self.filled {
                    *m = 0;
                }
            }
        }
        let held = self.filled.iter().map(|(_, m)| *m).max().unwrap_or(0);
        let upto = if held == 0 {
            self.queue.len()
        } else {
            self.at
                .get(self.at.len().saturating_sub(held))
                .map_or(0, |&p| p.wrapping_sub(self.base))
                .min(self.queue.len())
        };
        self.release(upto, out)
    }

    /// Lets the first `n` bytes go on.
    fn release(&mut self, n: usize, out: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if n == 0 {
            return Ok(());
        }
        let n = n.min(self.queue.len());
        out(self.queue.get(..n).unwrap_or_default())?;
        self.queue.drain(..n);
        self.base += n;
        Ok(())
    }

    /// A message's end: everything goes, no value begun.
    fn end_message(&mut self, out: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        let n = self.queue.len();
        self.release(n, out)?;
        for (_, m) in &mut self.filled {
            *m = 0;
        }
        self.at.clear();
        Ok(())
    }
}

/// A granted host's flow: the guest's TLS ended here, the host's made from here.
struct Ended {
    /// The name its TLS was ended under, lowered.
    sni: String,
    guest: ServerConnection,
    host: ClientConnection,
    /// The guest's plaintext not yet taken.
    pending: Vec<u8>,
    request: Request,
    response: Response,
    /// What the host owes for the request it was last given, until its final response ends.
    owed: Option<Owed>,
    /// Whether any of that response has gone to the guest.
    answered: bool,
    /// The values filled into that request, masked in its response.
    mask: Mask,
    /// Why the host's TLS failed, before anything of it reached the guest.
    failed: Option<String>,
}

impl Ended {
    /// The guest's TLS ended under a leaf for `sni`, its ClientHello `accepted` and the bytes
    /// after it `rest`; the host's begun.
    fn new(
        sni: &str,
        accepted: rustls::server::Accepted,
        rest: &[u8],
        run: &mut Run<'_>,
        q: &mut Queues<'_>,
    ) -> Result<Ended, Said> {
        let (cert, key) = run.credentials.ca.leaf(sni).map_err(|e| {
            say(sni, format_args!("no leaf: {e}"));
            Said::Reset
        })?;
        let config = guest_config(cert, key).map_err(|e| {
            say(sni, format_args!("no TLS of the guest's: {e}"));
            Said::Reset
        })?;
        let mut guest = match accepted.into_connection(config) {
            Ok(guest) => guest,
            Err((e, mut alert)) => {
                say(sni, format_args!("the guest's TLS refused: {e}"));
                return Err(match alert.write_all(&mut Onto(&mut *q.to_guest)) {
                    Ok(()) => Said::Close,
                    Err(_) => Said::Reset,
                });
            }
        };
        let name = ServerName::try_from(sni.to_string()).map_err(|_| Said::Reset)?;
        let mut host = ClientConnection::new(run.credentials.upstream.0.clone(), name).map_err(|e| {
            say(sni, format_args!("no TLS of the host's: {e}"));
            Said::Reset
        })?;
        // Their queues are the connection's, whose window bounds them; rustls's own bounds
        // would only hold back what the window already lets come.
        guest.set_buffer_limit(None);
        host.set_buffer_limit(None);
        let mut e = Ended {
            sni: sni.to_string(),
            guest,
            host,
            pending: Vec::new(),
            request: Request::Head,
            response: Response::Head(Vec::new()),
            owed: None,
            answered: false,
            mask: Mask::default(),
            failed: None,
        };
        if let Err(said) = e.take_guest(rest, q) {
            return Err(said);
        }
        Ok(e)
    }

    fn held(&self) -> usize {
        let parked = match &self.request {
            Request::Parked(p) => p.head.len(),
            _ => 0,
        };
        self.pending.len() + parked
    }

    /// The guest's TLS bytes into its connection, its plaintext into `pending`. A failure of
    /// the guest's TLS is answered with its alert, and the flow closes.
    fn take_guest(&mut self, bytes: &[u8], q: &mut Queues<'_>) -> Result<(), Said> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let read = self.guest.read_tls(&mut rest);
            let processed = read.as_ref().ok().map(|_| self.guest.process_new_packets());
            match (read, processed) {
                (Ok(0), _) => break,
                (Ok(_), Some(Ok(_))) => {}
                (Err(e), _) => return Err(self.guest_failed(&e.to_string(), q)),
                (Ok(_), Some(Err(e))) => return Err(self.guest_failed(&e.to_string(), q)),
                (Ok(_), None) => break,
            }
            if plain(&mut self.guest, &mut self.pending) {
                self.guest_closed(q);
            }
        }
        Ok(())
    }

    fn guest_failed(&mut self, why: &str, q: &mut Queues<'_>) -> Said {
        say(&self.sni, format_args!("the guest's TLS failed: {why}"));
        self.request = Request::Done;
        self.host.send_close_notify();
        match (send(&mut self.guest, q.to_guest), send(&mut self.host, q.to_host)) {
            (Ok(()), Ok(())) => Said::Close,
            _ => Said::Reset,
        }
    }

    /// The guest's TLS ended its side: the host's is told, once what came before goes.
    fn guest_closed(&mut self, q: &mut Queues<'_>) {
        self.host.send_close_notify();
        let _ = send(&mut self.host, q.to_host);
    }

    fn from_guest(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        if let Err(said) = self.take_guest(bytes, q) {
            return said;
        }
        self.serve(run, q)
    }

    fn guest_ended(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        let said = self.serve(run, q);
        self.guest_closed(q);
        said
    }

    /// The host's TLS bytes: its plaintext answered to the guest as it comes.
    fn from_host(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        let mut rest = bytes;
        let mut closed = false;
        while !rest.is_empty() {
            let read = self.host.read_tls(&mut rest);
            let processed = read.as_ref().ok().map(|_| self.host.process_new_packets());
            match (read, processed) {
                (Ok(0), _) | (Ok(_), None) => break,
                (Ok(_), Some(Ok(_))) => {}
                (Err(e), _) => return self.host_failed(&e.to_string(), run, q),
                (Ok(_), Some(Err(e))) => return self.host_failed(&e.to_string(), run, q),
            }
            let mut got = Vec::new();
            closed |= plain(&mut self.host, &mut got);
            if let Err(Broken(why)) = self.respond(&got, run) {
                say(&self.sni, format_args!("the host's response is broken: {why}"));
                return Said::Reset;
            }
        }
        if closed {
            return self.host_done(q);
        }
        self.serve(run, q)
    }

    fn host_ended(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        let _ = self.host.read_tls(&mut &[][..]);
        let _ = self.host.process_new_packets();
        let mut got = Vec::new();
        plain(&mut self.host, &mut got);
        if let Err(Broken(why)) = self.respond(&got, run) {
            say(&self.sni, format_args!("the host's response is broken: {why}"));
            return Said::Reset;
        }
        self.host_done(q)
    }

    /// The host's TLS failed: the guest is told 502 where nothing of its answer has reached it
    /// yet, else its connection is reset.
    fn host_failed(&mut self, why: &str, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        say(&self.sni, format_args!("the host's TLS failed: {why}"));
        let _ = send(&mut self.host, q.to_host);
        if self.answered {
            return Said::Reset;
        }
        self.failed = Some(why.to_string());
        match self.request {
            Request::Parked(_) | Request::Sent | Request::Body(_) => {
                self.refuse(Refusal::Upstream(why.to_string()), q)
            }
            _ => self.serve(run, q),
        }
    }

    /// The host has ended its side: a response read to its end ends there, one owed and not
    /// begun is answered 502, and the guest's TLS ends.
    fn host_done(&mut self, q: &mut Queues<'_>) -> Said {
        if matches!(self.response, Response::Body(Framing::Close) | Response::Tunnel) {
            let Ended { mask, guest, .. } = self;
            if mask.end_message(&mut |b| guest.writer().write_all(b)).is_err() {
                return Said::Reset;
            }
            self.exchange_ended();
        }
        if self.owed.is_some() && !self.answered && !matches!(self.request, Request::Done) {
            return self.refuse(Refusal::Upstream("the host ended before it answered".into()), q);
        }
        self.request = Request::Done;
        self.guest.send_close_notify();
        match (send(&mut self.guest, q.to_guest), send(&mut self.host, q.to_host)) {
            (Ok(()), Ok(())) => Said::Close,
            _ => Said::Reset,
        }
    }

    /// Answers the guest itself with `refusal`, and ends both sides.
    fn refuse(&mut self, refusal: Refusal, q: &mut Queues<'_>) -> Said {
        let (status, _) = refusal.status();
        say(&self.sni, format_args!("answered {status}: {refusal}"));
        self.request = Request::Done;
        self.pending.clear();
        let written = self.guest.writer().write_all(&refusal.response());
        self.guest.send_close_notify();
        self.host.send_close_notify();
        match (
            written,
            send(&mut self.guest, q.to_guest),
            send(&mut self.host, q.to_host),
        ) {
            (Ok(()), Ok(()), Ok(())) => Said::Close,
            _ => Said::Reset,
        }
    }

    /// Takes what it can of the guest's requests, and sends what each side is to have.
    fn serve(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        loop {
            match self.advance(run) {
                Ok(true) => {}
                Ok(false) => break,
                Err(refusal) => return self.refuse(refusal, q),
            }
        }
        match (send(&mut self.guest, q.to_guest), send(&mut self.host, q.to_host)) {
            (Ok(()), Ok(())) if matches!(self.request, Request::Done) && self.owed.is_none() => Said::Close,
            (Ok(()), Ok(())) => Said::Going,
            _ => Said::Reset,
        }
    }

    /// One step of the requests' side: whether it moved.
    fn advance(&mut self, run: &mut Run<'_>) -> Result<bool, Refusal> {
        match std::mem::replace(&mut self.request, Request::Done) {
            Request::Head => match self.read_head(run)? {
                Some(parked) => {
                    self.request = Request::Parked(Box::new(parked));
                    Ok(true)
                }
                None => {
                    self.request = Request::Head;
                    Ok(false)
                }
            },
            Request::Parked(parked) => self.go(parked, run),
            Request::Body(mut framing) => {
                let Ended { host, pending, .. } = self;
                let (taken, ended) = walk(
                    &mut framing,
                    pending,
                    crate::tcp::TO_HOST,
                    &mut |piece| match piece {
                        Piece::Data(b) | Piece::Frame(b) => host.writer().write_all(b),
                    },
                )
                .map_err(|Broken(why)| Refusal::Malformed(why))?;
                pending.drain(..taken.min(pending.len()));
                self.request = if !ended {
                    Request::Body(framing)
                } else if matches!(self.response, Response::Tunnel) {
                    Request::Tunnel
                } else if self.owed.is_some() {
                    Request::Sent
                } else {
                    Request::Head
                };
                Ok(ended)
            }
            Request::Sent => {
                self.request = if self.owed.is_some() {
                    Request::Sent
                } else {
                    Request::Head
                };
                Ok(matches!(self.request, Request::Head))
            }
            Request::Tunnel => {
                self.request = Request::Tunnel;
                if self.pending.is_empty() {
                    return Ok(false);
                }
                let bytes = std::mem::take(&mut self.pending);
                self.host
                    .writer()
                    .write_all(&bytes)
                    .map_err(|e| Refusal::Upstream(e.to_string()))?;
                Ok(false)
            }
            Request::Done => {
                self.pending.clear();
                Ok(false)
            }
        }
    }

    /// A request's head, if it has come whole: its host and framing read, its hop-by-hop
    /// fields named, each placeholder that stands in a credential field toward its host found.
    fn read_head(&mut self, run: &Run<'_>) -> Result<Option<Parked>, Refusal> {
        // Empty lines before a request line are passed over (RFC 9112 §2.2).
        let blank = self.pending.chunks(2).take_while(|pair| *pair == b"\r\n").count();
        self.pending.drain(..(blank * 2).min(self.pending.len()));
        let Some(end) =
            head_end(&self.pending).map_err(|()| Refusal::Malformed("a line ends in a bare LF or CR"))?
        else {
            if self.pending.len() > crate::tcp::TO_HOST {
                return Err(Refusal::TooLarge);
            }
            return Ok(None);
        };
        let head: Vec<u8> = self.pending.drain(..end.min(self.pending.len())).collect();
        let lines = head.iter().filter(|&&b| b == b'\n').count();
        let mut read = vec![httparse::EMPTY_HEADER; lines];
        let mut request = httparse::Request::new(&mut read);
        match request.parse(&head) {
            Ok(httparse::Status::Complete(n)) if n == head.len() => {}
            _ => return Err(Refusal::Malformed("a head that is no HTTP/1.1 request's")),
        }
        let target = request.path.unwrap_or_default().to_string();
        let method = request.method.unwrap_or_default().to_string();
        let minor = request.version.unwrap_or(1);
        let fields =
            fields(&head, request.headers).ok_or(Refusal::Malformed("a field this side cannot place"))?;
        let host = request_host(&target, &head, &fields)?;
        let body = request_framing(&head, &fields, minor)?;
        let owed = Owed {
            head: method.eq_ignore_ascii_case("HEAD"),
            connect: method.eq_ignore_ascii_case("CONNECT"),
            upgrade: fields.iter().any(|f| f.name == "upgrade"),
        };
        let grants = &run.credentials.grants;
        if credential::decide(grants, &self.sni, &host, None) == Fill::Misdirected {
            return Err(Refusal::Misdirected {
                sni: self.sni.clone(),
                host,
            });
        }
        let edits = hop_by_hop(&head, &fields);
        let mut fills = Vec::new();
        for field in &fields {
            if edits
                .iter()
                .any(|(cut, _)| cut.start <= field.line.start && field.line.end <= cut.end)
            {
                continue;
            }
            let Some(range) = token(&head, field) else {
                continue;
            };
            let Some(token) = head.get(range.clone()).and_then(|t| std::str::from_utf8(t).ok()) else {
                continue;
            };
            if let Fill::With(reference) = credential::decide(grants, &self.sni, &host, Some(token)) {
                fills.push((range, reference));
            }
        }
        Ok(Some(Parked {
            head,
            edits,
            fills,
            host,
            body,
            owed,
        }))
    }

    /// A parked request goes once the host's TLS is up and each value it carries is leased:
    /// its head as it came but for its hop-by-hop fields and its placeholders, each filled in
    /// and audited; refused where a lease is; parked again while one is asked.
    fn go(&mut self, parked: Box<Parked>, run: &mut Run<'_>) -> Result<bool, Refusal> {
        if let Some(why) = &self.failed {
            return Err(Refusal::Upstream(why.clone()));
        }
        if self.host.is_handshaking() {
            self.request = Request::Parked(parked);
            return Ok(false);
        }
        let mut waiting = false;
        let mut asked: Vec<&str> = Vec::new();
        for (_, reference) in &parked.fills {
            if asked.contains(&reference.as_str()) {
                continue;
            }
            asked.push(reference);
            match run.credentials.leases.ask(reference, run.waiter, run.now) {
                Asked::Held(_) => {}
                Asked::Waiting => waiting = true,
                Asked::Refused(why) => {
                    return Err(Refusal::Unleased {
                        reference: reference.clone(),
                        why,
                    });
                }
            }
        }
        if waiting {
            self.request = Request::Parked(parked);
            return Ok(false);
        }
        let leases = &run.credentials.leases;
        let mut edits: Vec<(Range<usize>, &[u8])> = parked
            .edits
            .iter()
            .map(|(r, v)| (r.clone(), v.as_slice()))
            .collect();
        for (range, reference) in &parked.fills {
            let value = leases.held(reference, run.now).ok_or_else(|| Refusal::Unleased {
                reference: reference.clone(),
                why: lease::Refusal::Lapsed,
            })?;
            edits.push((range.clone(), value));
        }
        let head = rewrite(&parked.head, &edits);
        self.host
            .writer()
            .write_all(&head)
            .map_err(|e| Refusal::Upstream(e.to_string()))?;
        drop(head);
        let at = SystemTime::now();
        self.mask.filled.clear();
        for (_, reference) in &parked.fills {
            if !self.mask.filled.iter().any(|(r, _)| r == reference) {
                run.audit.push(Audit {
                    host: parked.host.clone(),
                    reference: reference.clone(),
                    at,
                });
                self.mask.filled.push((reference.clone(), 0));
            }
        }
        self.owed = Some(parked.owed);
        self.answered = false;
        self.request = match parked.body {
            Framing::Length(0) => Request::Sent,
            body => Request::Body(body),
        };
        Ok(true)
    }

    /// The host's plaintext `bytes`, answered to the guest as its responses' framing reads
    /// them, the values filled into their request masked.
    fn respond(&mut self, mut bytes: &[u8], run: &Run<'_>) -> Result<(), Broken> {
        let queued = |_: io::Error| Broken("its bytes could not be queued");
        while !bytes.is_empty() {
            if matches!(self.request, Request::Done) {
                return Ok(());
            }
            match &mut self.response {
                Response::Head(buf) => {
                    let Some(end) = head_end(&[buf.as_slice(), bytes].concat())
                        .map_err(|()| Broken("a line ends in a bare LF or CR"))?
                    else {
                        buf.extend_from_slice(bytes);
                        if buf.len() > crate::tcp::TO_GUEST {
                            return Err(Broken("a head past what the connection holds"));
                        }
                        return Ok(());
                    };
                    let take = end.saturating_sub(buf.len());
                    buf.extend_from_slice(bytes.get(..take).unwrap_or_default());
                    bytes = bytes.get(take..).unwrap_or_default();
                    let head = std::mem::take(buf);
                    self.response_head(head, run)?;
                }
                Response::Body(framing) => {
                    let Ended { mask, guest, .. } = self;
                    let mut piece = |p: Piece<'_>| match p {
                        Piece::Data(b) => mask.push(b, true, run, &mut |b| guest.writer().write_all(b)),
                        Piece::Frame(b) => mask.push(b, false, run, &mut |b| guest.writer().write_all(b)),
                    };
                    let (taken, ended) = walk(framing, bytes, crate::tcp::TO_GUEST, &mut piece)?;
                    bytes = bytes.get(taken..).unwrap_or_default();
                    if ended {
                        mask.end_message(&mut |b| guest.writer().write_all(b))
                            .map_err(queued)?;
                        self.response = Response::Head(Vec::new());
                        self.exchange_ended();
                    }
                }
                Response::Tunnel => {
                    let Ended { mask, guest, .. } = self;
                    mask.push(bytes, true, run, &mut |b| guest.writer().write_all(b))
                        .map_err(queued)?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// A response's head, whole: passed on, hop-by-hop fields cut and echoes masked, and what
    /// follows it read as its status and framing say.
    fn response_head(&mut self, head: Vec<u8>, run: &Run<'_>) -> Result<(), Broken> {
        let lines = head.iter().filter(|&&b| b == b'\n').count();
        let mut read = vec![httparse::EMPTY_HEADER; lines];
        let mut response = httparse::Response::new(&mut read);
        match response.parse(&head) {
            Ok(httparse::Status::Complete(n)) if n == head.len() => {}
            _ => return Err(Broken("a head that is no HTTP/1.1 response's")),
        }
        let status = response.code.unwrap_or(0);
        let fields = fields(&head, response.headers).ok_or(Broken("a field this side cannot place"))?;
        let owed = self.owed.unwrap_or_default();
        let interim = (100..200).contains(&status) && status != 101;
        let tunnel = (status == 101 && owed.upgrade) || (owed.connect && (200..300).contains(&status));
        if status == 101 && !owed.upgrade {
            return Err(Broken("a switch of protocols no request asked"));
        }
        let framing = if tunnel || interim {
            Framing::Length(0)
        } else {
            response_framing(status, owed, &head, &fields)?
        };
        let cut = hop_by_hop(&head, &fields);
        let edits: Vec<(Range<usize>, &[u8])> = cut.iter().map(|(r, v)| (r.clone(), v.as_slice())).collect();
        let passed = rewrite(&head, &edits);
        let Ended { mask, guest, .. } = self;
        let mut out = |b: &[u8]| guest.writer().write_all(b);
        mask.push(&passed, true, run, &mut out)
            .and_then(|()| mask.end_message(&mut out))
            .map_err(|_| Broken("its bytes could not be queued"))?;
        self.answered = true;
        self.response = if tunnel {
            Response::Tunnel
        } else if interim {
            Response::Head(Vec::new())
        } else if framing == Framing::Length(0) {
            self.exchange_ended();
            Response::Head(Vec::new())
        } else {
            Response::Body(framing)
        };
        if tunnel && matches!(self.request, Request::Sent) {
            self.request = Request::Tunnel;
        }
        Ok(())
    }

    /// The final response to the request under way has ended: nothing is owed, its values are
    /// masked no more, and the next request may come.
    fn exchange_ended(&mut self) {
        self.owed = None;
        self.mask.filled.clear();
        if matches!(self.request, Request::Sent) {
            self.request = Request::Head;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::num::NonZeroU64;
    use std::sync::mpsc;
    use std::time::Duration;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::CipherSuite;

    use crate::{Grant, ca, lease};

    const GRANTED: &str = "api.example.test";
    /// Granted too, but for the other credential alone.
    const OTHER: &str = "other.example.test";
    /// No grant names it.
    const FREE: &str = "free.example.test";
    const CLAUDE: &str = "anthropic/claude-max";
    const ELSEWHERE: &str = "openai/chatgpt";
    const PLACEHOLDER: &str =
        "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ELSEWHERE_PLACEHOLDER: &str =
        "kiln-placeholder-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    /// A value of another length than its placeholder, so that its mask is a part of it.
    const VALUE: &[u8] = b"sk-ant-api03-not-a-real-value-0123456789";

    /// The window the bench's connection advertises: small, so that a body meets it.
    const WINDOW: usize = 256 << 10;

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A day's validity, within what every platform's verifier takes of a server's certificate
    /// (the registry's tests give theirs the same).
    fn params(names: &[&str]) -> CertificateParams {
        let mut p = CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
        p.not_before = time::OffsetDateTime::from_unix_timestamp(now() - 3600).unwrap();
        p.not_after = time::OffsetDateTime::from_unix_timestamp(now() + 86400).unwrap();
        p
    }

    /// A test root, which the injector's host side is given to trust as a host's.
    fn root(name: &str) -> CertifiedIssuer<'static, KeyPair> {
        let mut p = params(&[]);
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        p.distinguished_name.push(DnType::CommonName, name);
        CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap()
    }

    /// The echo server's TLS, any suite: a leaf for the three hosts that `root` signs.
    fn echo_config(root: &CertifiedIssuer<'static, KeyPair>) -> Arc<ServerConfig> {
        let mut p = params(&[GRANTED, OTHER, FREE]);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let key = KeyPair::generate().unwrap();
        let cert = p.signed_by(&key, root).unwrap();
        Arc::new(
            ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                )
                .unwrap(),
        )
    }

    /// What the echo server had of a request, and its TLS's suite.
    struct Had {
        request: Vec<u8>,
        suite: Option<CipherSuite>,
    }

    /// An HTTPS server that answers each request with itself, its head and then its body,
    /// the body read and answered a piece at a time, `slow`ly where asked, telling the test
    /// what it had.
    fn echo(config: Arc<ServerConfig>, slow: bool) -> (SocketAddr, mpsc::Receiver<Had>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { return };
                let (config, tx) = (config.clone(), tx.clone());
                std::thread::spawn(move || answer(config, tcp, tx, slow));
            }
        });
        (addr, rx)
    }

    fn answer(config: Arc<ServerConfig>, tcp: TcpStream, tx: mpsc::Sender<Had>, slow: bool) {
        let mut tls = rustls::StreamOwned::new(ServerConnection::new(config).unwrap(), tcp);
        loop {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match tls.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => return,
                }
            }
            let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
            let len: usize = text
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", head.len() + len);
            if tls
                .write_all(answer.as_bytes())
                .and_then(|()| tls.write_all(&head))
                .is_err()
            {
                return;
            }
            let mut request = head;
            let (mut left, mut buf) = (len, vec![0u8; 16 << 10]);
            while left > 0 {
                let n = match tls.read(&mut buf[..left.min(16 << 10)]) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                request.extend_from_slice(&buf[..n]);
                if tls.write_all(&buf[..n]).is_err() {
                    return;
                }
                left -= n;
                if slow {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let suite = tls.conn.negotiated_cipher_suite().map(|s| s.suite());
            let _ = tls.flush();
            let _ = tx.send(Had { request, suite });
        }
    }

    /// A run's credentials: one toward the granted host, the other toward the other host, the
    /// injector's host side trusting `root` beside the host's own roots.
    fn credentials(root: &CertifiedIssuer<'static, KeyPair>) -> Credentials {
        Credentials {
            grants: vec![
                Grant {
                    reference: CLAUDE.into(),
                    toward: vec![GRANTED.into()],
                    placeholder: PLACEHOLDER.into(),
                },
                Grant {
                    reference: ELSEWHERE.into(),
                    toward: vec![OTHER.into()],
                    placeholder: ELSEWHERE_PLACEHOLDER.into(),
                },
            ],
            ca: ca::Ca::new(ca::Key::generate().unwrap(), &[GRANTED, OTHER]).unwrap(),
            leases: lease::Leases::new(vec![CLAUDE.into(), ELSEWHERE.into()]),
            upstream: Upstream::new(vec![root.der().clone()]).unwrap(),
        }
    }

    /// Answers each lease asked, as the run host relays its broker's, with the test's value.
    fn lease_all(credentials: &mut Credentials) -> usize {
        let asks = credentials.leases.asks();
        for ask in &asks {
            let (id, reference) = lease::decode_ask(ask).unwrap();
            assert_eq!(reference, CLAUDE, "a lease of a credential no request carried");
            let life = NonZeroU64::new(60_000_000_000).unwrap();
            let answer = lease::encode_answer(id, Ok((life, VALUE))).unwrap();
            assert!(credentials.leases.answered(&answer, Instant::now()));
        }
        asks.len()
    }

    /// A guest's TLS client of `name`, trusting `trusts` alone, offering `suites` (rustls's
    /// all where none are named).
    fn guest(
        trusts: CertificateDer<'static>,
        name: &str,
        suites: Option<Vec<rustls::SupportedCipherSuite>>,
    ) -> ClientConnection {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(trusts).unwrap();
        let mut provider = aws_lc_rs::default_provider();
        if let Some(suites) = suites {
            provider.cipher_suites = suites;
        }
        let config = ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut c =
            ClientConnection::new(Arc::new(config), ServerName::try_from(name.to_string()).unwrap()).unwrap();
        c.set_buffer_limit(None);
        c
    }

    /// At most so many bytes, written.
    struct Limited<'a>(&'a mut Vec<u8>, usize);

    impl Write for Limited<'_> {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let n = b.len().min(self.1.saturating_sub(self.0.len()));
            if n == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.0.extend_from_slice(&b[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A guest's TLS client, the injector's flow and its connection's queues as tcp.rs keeps
    /// them, and the host's socket: the guest's bytes given while the window has room for them
    /// (its queue to the host and what the flow holds), the host's read while the queue to the
    /// guest has room, the guest given a piece of that queue at a time.
    struct Bench {
        guest: ClientConnection,
        flow: Option<Flow>,
        to_host: Ring,
        to_guest: Ring,
        sock: TcpStream,
        host_ended: bool,
        closed: bool,
        /// What the guest read, and the most the flow held.
        got: Vec<u8>,
        held_most: usize,
        audit: Vec<Audit>,
        passed: bool,
    }

    impl Bench {
        fn new(guest: ClientConnection, to: SocketAddr) -> Bench {
            let sock = TcpStream::connect(to).unwrap();
            sock.set_nonblocking(true).unwrap();
            Bench {
                guest,
                flow: Some(Flow::new()),
                to_host: Ring::new(),
                to_guest: Ring::new(),
                sock,
                host_ended: false,
                closed: false,
                got: Vec::new(),
                held_most: 0,
                audit: Vec::new(),
                passed: false,
            }
        }

        fn held(&self) -> usize {
            self.flow.as_ref().map_or(0, Flow::held)
        }

        /// The flow's step with the run, its say taken as tcp.rs takes it.
        fn step(
            &mut self,
            credentials: &mut Credentials,
            host: Option<&[u8]>,
            f: impl FnOnce(&mut Flow, &mut Run<'_>, &mut Queues<'_>) -> Said,
        ) {
            let Some(flow) = self.flow.as_mut() else { return };
            let mut run = Run {
                credentials,
                waiter: 7,
                now: Instant::now(),
                audit: Vec::new(),
            };
            let said = f(
                flow,
                &mut run,
                &mut Queues {
                    to_host: &mut self.to_host,
                    to_guest: &mut self.to_guest,
                },
            );
            self.audit.append(&mut run.audit);
            self.held_most = self.held_most.max(self.held());
            match said {
                Said::Going => {}
                Said::Pass(seen) => {
                    self.flow = None;
                    self.passed = true;
                    self.to_host.extend(&seen).unwrap();
                    if let Some(host) = host {
                        self.to_guest.extend(host).unwrap();
                    }
                }
                Said::Close => self.closed = true,
                Said::Reset => panic!("the flow was reset"),
            }
        }

        /// One turn of each side; whether anything moved.
        fn turn(&mut self, credentials: &mut Credentials) -> bool {
            let mut moved = false;
            // The guest's bytes, as the window takes them.
            let room = WINDOW.saturating_sub(self.to_host.len() + self.held());
            let mut bytes = Vec::new();
            while self.guest.wants_write() && bytes.len() < room {
                if !matches!(self.guest.write_tls(&mut Limited(&mut bytes, room)), Ok(n) if n > 0) {
                    break;
                }
            }
            if !bytes.is_empty() {
                moved = true;
                if self.flow.is_some() {
                    self.step(credentials, None, |f, run, q| f.from_guest(&bytes, run, q));
                } else {
                    self.to_host.extend(&bytes).unwrap();
                }
            }
            // To the host, as its socket takes them.
            if !self.to_host.is_empty() {
                let (a, _) = self.to_host.as_slices();
                match (&self.sock).write(a) {
                    Ok(n) if n > 0 => {
                        self.to_host.drain(n);
                        moved = true;
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("the host's socket: {e}"),
                }
            }
            // The host's, while the queue to the guest has room.
            if !self.host_ended && self.to_guest.len() < WINDOW {
                let mut buf = vec![0u8; WINDOW - self.to_guest.len()];
                match (&self.sock).read(&mut buf) {
                    Ok(0) => {
                        self.host_ended = true;
                        moved = true;
                        self.step(credentials, None, |f, run, q| f.host_ended(run, q));
                    }
                    Ok(n) => {
                        moved = true;
                        let got = &buf[..n];
                        if self.flow.is_some() {
                            self.step(credentials, Some(got), |f, run, q| f.from_host(got, run, q));
                        } else {
                            self.to_guest.extend(got).unwrap();
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("the host's socket: {e}"),
                }
            }
            // To the guest, a TLS record's worth at a time.
            if !self.to_guest.is_empty() {
                let (a, _) = self.to_guest.as_slices();
                let piece = &a[..a.len().min(16 << 10)];
                let n = self.guest.read_tls(&mut &piece[..]).unwrap();
                self.to_guest.drain(n);
                self.guest
                    .process_new_packets()
                    .map_err(|e| format!("the guest's TLS: {e}"))
                    .unwrap();
                let mut reader = self.guest.reader();
                let _ = reader.read_to_end(&mut self.got);
                moved |= n > 0;
            }
            moved
        }

        /// Turns until `done` says so of what the guest read, its leases answered as they are
        /// asked where `lease` says; a turn that moves nothing waits a little for the host.
        fn until(&mut self, credentials: &mut Credentials, lease: bool, done: impl Fn(&Bench) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !done(self) {
                assert!(
                    Instant::now() < deadline,
                    "no end: the guest read {:?}",
                    String::from_utf8_lossy(&self.got)
                );
                let moved = self.turn(credentials);
                if lease && lease_all(credentials) > 0 {
                    for r in credentials.leases.resumed() {
                        assert_eq!(r.waiter, 7);
                    }
                    self.step(credentials, None, |f, run, q| f.resume(run, q));
                } else if !moved {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// Whether `got` holds a whole response, by its Content-Length.
    fn whole(got: &[u8]) -> bool {
        let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap());
        got.len() >= end + 4 + len
    }

    /// `haystack` holds `needle`.
    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// `request` with its placeholder, `from`, made `to`.
    fn swapped(request: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let at = request.windows(from.len()).position(|w| w == from).unwrap();
        [&request[..at], to, &request[at + from.len()..]].concat()
    }

    /// The mask an echo of the value is given: its placeholder's bytes, cycled to its length.
    fn mask() -> Vec<u8> {
        PLACEHOLDER.bytes().cycle().take(VALUE.len()).collect()
    }

    /// Do: send a granted host a request with the credential's placeholder in `x-api-key`,
    /// then on the same connection one with it as a bearer token, the echo server answering
    /// each with itself. Expect: the first parked until its lease is answered, nothing reaching
    /// the host before; each arriving whole with the real value where its placeholder stood
    /// and every other byte as the guest sent it; the echoes of the value back to the guest
    /// masked with its placeholder's bytes, the value never reaching the guest; each fill
    /// audited by its host and reference, no value in the record; both sides AES-256-GCM.
    #[test]
    fn a_granted_hosts_request_carries_the_value_and_its_echo_comes_back_masked() {
        let root = root("shards test root");
        let (addr, had) = echo(echo_config(&root), false);
        let mut credentials = credentials(&root);
        let mut bench = Bench::new(guest(credentials.ca.der().to_vec().into(), GRANTED, None), addr);
        let first = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: {GRANTED}\r\nx-api-key: {PLACEHOLDER}\r\nContent-Length: 5\r\n\r\nhello"
        )
        .into_bytes();
        bench.guest.writer().write_all(&first).unwrap();
        // Parked on its lease, asked once the host's TLS is up: nothing reaches the host but
        // that handshake.
        let deadline = Instant::now() + Duration::from_secs(30);
        let asks = loop {
            let moved = bench.turn(&mut credentials);
            let asks = credentials.leases.asks();
            if !asks.is_empty() {
                break asks;
            }
            assert!(Instant::now() < deadline, "no lease asked");
            if !moved {
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        assert_eq!(asks.len(), 1, "one lease asked for the request");
        assert!(
            bench.held() > 0,
            "the request was not held while its lease was asked"
        );
        assert!(
            had.try_recv().is_err(),
            "a request reached the host before its value was leased"
        );
        let (id, reference) = lease::decode_ask(&asks[0]).unwrap();
        assert_eq!(reference, CLAUDE);
        let life = NonZeroU64::new(60_000_000_000).unwrap();
        assert!(credentials.leases.answered(
            &lease::encode_answer(id, Ok((life, VALUE))).unwrap(),
            Instant::now()
        ));
        assert_eq!(credentials.leases.resumed().len(), 1);
        bench.step(&mut credentials, None, |f, run, q| f.resume(run, q));
        bench.until(&mut credentials, true, |b| whole(&b.got));
        let one = had.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(one.request, swapped(&first, PLACEHOLDER.as_bytes(), VALUE));
        assert_eq!(one.suite, Some(CipherSuite::TLS13_AES_256_GCM_SHA384));
        assert!(!holds(&bench.got, VALUE), "the value reached the guest");
        assert!(
            holds(&bench.got, &mask()),
            "the echo was not masked with its placeholder"
        );
        assert_eq!(
            bench.guest.negotiated_cipher_suite().map(|s| s.suite()),
            Some(CipherSuite::TLS13_AES_256_GCM_SHA384)
        );
        // The same connection's next request, its placeholder a bearer token, its lease held.
        bench.got.clear();
        let second = format!(
            "GET /v1/models HTTP/1.1\r\nHost: {GRANTED}:443\r\nAuthorization: bEaReR {PLACEHOLDER}\r\n\r\n"
        )
        .into_bytes();
        bench.guest.writer().write_all(&second).unwrap();
        bench.until(&mut credentials, true, |b| whole(&b.got));
        let two = had.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(two.request, swapped(&second, PLACEHOLDER.as_bytes(), VALUE));
        assert!(!holds(&bench.got, VALUE), "the value reached the guest");
        assert!(
            credentials.leases.asks().is_empty(),
            "a held lease was asked again"
        );
        assert_eq!(bench.audit.len(), 2);
        for record in &bench.audit {
            assert_eq!(
                (record.host.as_str(), record.reference.as_str()),
                (GRANTED, CLAUDE)
            );
            let said = record.to_string();
            assert!(said.contains(CLAUDE) && said.contains(GRANTED), "{said}");
            assert!(
                !holds(said.as_bytes(), VALUE) && !said.contains(PLACEHOLDER),
                "{said}"
            );
        }
    }

    /// Do: send a host no grant names a request with a placeholder, then a granted host one
    /// with the placeholder of a credential granted toward another host alone. Expect: the
    /// first's flow passes as it came, its TLS the host's own; each arrives with its
    /// placeholder, no lease asked and nothing filled in or audited (f32, f33).
    #[test]
    fn a_placeholder_toward_an_ungranted_host_carries_no_credential() {
        let root = root("shards test root");
        let (addr, had) = echo(echo_config(&root), false);
        let mut credentials = credentials(&root);
        let mut free = Bench::new(guest(root.der().clone(), FREE, None), addr);
        let request =
            format!("GET / HTTP/1.1\r\nHost: {FREE}\r\nx-api-key: {PLACEHOLDER}\r\n\r\n").into_bytes();
        free.guest.writer().write_all(&request).unwrap();
        free.until(&mut credentials, false, |b| whole(&b.got));
        assert!(
            free.passed,
            "a flow to an ungranted host was not passed as it came"
        );
        assert_eq!(
            had.recv_timeout(Duration::from_secs(10)).unwrap().request,
            request
        );
        let mut granted = Bench::new(guest(credentials.ca.der().to_vec().into(), GRANTED, None), addr);
        let request =
            format!("GET / HTTP/1.1\r\nHost: {GRANTED}\r\nx-api-key: {ELSEWHERE_PLACEHOLDER}\r\n\r\n")
                .into_bytes();
        granted.guest.writer().write_all(&request).unwrap();
        granted.until(&mut credentials, false, |b| whole(&b.got));
        assert!(!granted.passed);
        assert_eq!(
            had.recv_timeout(Duration::from_secs(10)).unwrap().request,
            request
        );
        assert!(
            credentials.leases.asks().is_empty(),
            "a lease asked for no grant's request"
        );
        assert!(free.audit.is_empty() && granted.audit.is_empty());
    }

    /// Do: send a granted host's connection a request for another host, with the placeholder.
    /// Expect: answered 421 by the injector, the connection closed, nothing reaching the host
    /// and nothing leased (RFC 9110 §7.4, f44).
    #[test]
    fn a_request_for_a_host_that_is_not_the_sni_is_answered_421() {
        let root = root("shards test root");
        let (addr, had) = echo(echo_config(&root), false);
        let mut credentials = credentials(&root);
        let mut bench = Bench::new(guest(credentials.ca.der().to_vec().into(), GRANTED, None), addr);
        let request =
            format!("GET / HTTP/1.1\r\nHost: {OTHER}\r\nx-api-key: {PLACEHOLDER}\r\n\r\n").into_bytes();
        bench.guest.writer().write_all(&request).unwrap();
        bench.until(&mut credentials, false, |b| whole(&b.got));
        assert!(
            bench.got.starts_with(b"HTTP/1.1 421 Misdirected Request\r\n"),
            "{}",
            String::from_utf8_lossy(&bench.got)
        );
        assert!(bench.closed);
        assert!(credentials.leases.asks().is_empty());
        assert!(
            had.recv_timeout(Duration::from_millis(200)).is_err(),
            "a misdirected request reached the host"
        );
    }

    /// Do: send a granted host a body of 8 MiB with the placeholder in the head, through a
    /// window of 256 KiB, to a host that reads slowly and echoes it. Expect: the body arrives
    /// unchanged and its echo comes back unchanged but for the masked value, the injector
    /// holding none of the body as it streams, its queues never past the window and a record.
    #[test]
    fn a_large_body_streams_through_unchanged_under_backpressure() {
        let root = root("shards test root");
        let (addr, had) = echo(echo_config(&root), true);
        let mut credentials = credentials(&root);
        let mut bench = Bench::new(guest(credentials.ca.der().to_vec().into(), GRANTED, None), addr);
        let body: Vec<u8> = (0..8u32 << 20).map(|i| (i % 251) as u8).collect();
        let head = format!(
            "PUT /upload HTTP/1.1\r\nHost: {GRANTED}\r\nx-api-key: {PLACEHOLDER}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bench.guest.writer().write_all(&head).unwrap();
        bench.guest.writer().write_all(&body).unwrap();
        bench.until(&mut credentials, true, |b| {
            !b.guest.is_handshaking() && b.held() == 0 && b.audit.len() == 1
        });
        // From its head on, the body streams: nothing of it held.
        bench.held_most = 0;
        bench.until(&mut credentials, true, |b| whole(&b.got));
        assert_eq!(bench.held_most, 0, "the injector held the body");
        let had = had.recv_timeout(Duration::from_secs(10)).unwrap();
        let sent = swapped(&head, PLACEHOLDER.as_bytes(), VALUE);
        assert!(
            had.request.len() == sent.len() + body.len(),
            "the host had {} bytes",
            had.request.len()
        );
        assert!(
            had.request[..sent.len()] == sent[..] && had.request[sent.len()..] == body[..],
            "the request changed on its way"
        );
        let echoed = &bench.got[bench.got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
        let masked = swapped(&head, PLACEHOLDER.as_bytes(), &mask());
        assert!(
            echoed[..masked.len()] == masked[..],
            "the echoed head is not the masked one"
        );
        assert!(
            echoed[masked.len()..] == body[..],
            "the echoed body changed on its way back"
        );
    }

    /// Do: a guest whose TLS offers AES-128-GCM alone. Expect: refused at its handshake, the
    /// flow closing, nothing reaching the host (D242: AES-256-GCM alone on the guest's side).
    #[test]
    fn a_guest_offering_no_aes_256_gcm_is_refused() {
        let root = root("shards test root");
        let (addr, had) = echo(echo_config(&root), false);
        let mut credentials = credentials(&root);
        let only = vec![aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256];
        let mut bench = Bench::new(
            guest(credentials.ca.der().to_vec().into(), GRANTED, Some(only)),
            addr,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut refused = None;
        while refused.is_none() && Instant::now() < deadline {
            let room = WINDOW;
            let mut bytes = Vec::new();
            while bench.guest.wants_write() {
                if !matches!(bench.guest.write_tls(&mut Limited(&mut bytes, room)), Ok(n) if n > 0) {
                    break;
                }
            }
            if !bytes.is_empty() {
                bench.step(&mut credentials, None, |f, run, q| f.from_guest(&bytes, run, q));
            }
            if !bench.to_guest.is_empty() {
                let (a, _) = bench.to_guest.as_slices();
                let n = bench.guest.read_tls(&mut &a[..]).unwrap();
                bench.to_guest.drain(n);
                refused = bench.guest.process_new_packets().err();
            }
        }
        assert!(refused.is_some(), "a guest offering no AES-256-GCM was served");
        assert!(bench.closed);
        assert!(had.recv_timeout(Duration::from_millis(200)).is_err());
    }

    /// The fields httparse reads of `head`, where each lies.
    fn parsed(head: &[u8]) -> Vec<Field> {
        let mut read = vec![httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut read);
        request.parse(head).unwrap();
        fields(head, request.headers).unwrap()
    }

    /// A request's framing as RFC 9112 §6.3 reads it, its smuggling shapes refused (f43):
    /// Transfer-Encoding beside Content-Length, a last coding other than chunked, chunked twice,
    /// a coding in HTTP/1.0, and lengths that disagree; none is a body of none.
    #[test]
    fn a_requests_framing_is_read_strictly() {
        let framing = |fields: &str, minor: u8| {
            let head = format!("POST / HTTP/1.{minor}\r\nHost: a\r\n{fields}\r\n").into_bytes();
            request_framing(&head, &parsed(&head), minor)
        };
        assert_eq!(framing("", 1), Ok(Framing::Length(0)));
        assert_eq!(framing("Content-Length: 12\r\n", 1), Ok(Framing::Length(12)));
        assert_eq!(
            framing("Content-Length: 12, 12\r\nContent-Length: 12\r\n", 1),
            Ok(Framing::Length(12))
        );
        assert_eq!(
            framing("Transfer-Encoding: gzip, Chunked\r\n", 1),
            Ok(Framing::Chunked(Chunk::Size(Vec::new())))
        );
        for refused in [
            "Transfer-Encoding: chunked\r\nContent-Length: 3\r\n",
            "Transfer-Encoding: chunked, gzip\r\n",
            "Transfer-Encoding: chunked, chunked\r\n",
            "Content-Length: 12\r\nContent-Length: 13\r\n",
            "Content-Length: -1\r\n",
            "Content-Length: 1e3\r\n",
        ] {
            assert!(
                matches!(framing(refused, 1), Err(Refusal::Malformed(_))),
                "{refused:?} taken"
            );
        }
        assert!(matches!(
            framing("Transfer-Encoding: chunked\r\n", 0),
            Err(Refusal::Malformed(_))
        ));
    }

    /// A head's lines end in CRLF alone: one ended by a bare LF or CR is refused, so that no host
    /// that takes a lone LF reads another message than this side passed on (RFC 9112 §2.2).
    #[test]
    fn a_heads_lines_end_in_crlf_alone() {
        assert_eq!(head_end(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nrest"), Ok(Some(27)));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\nHost: a\r\n"), Ok(None));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\nHost: a\r"), Ok(None));
        assert_eq!(head_end(b"GET / HTTP/1.1\nHost: a\r\n\r\n"), Err(()));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\nX: a\rb\r\n\r\n"), Err(()));
    }

    /// A chunk's size line is RFC 9112 §7.1's alone: a size past a u64, no size, a control in
    /// its extensions (a lone LF there being where another reader could end the line) are no
    /// size; a chunked body walks to its end through its trailers, its data given as data.
    #[test]
    fn chunked_bodies_are_read_strictly() {
        assert_eq!(chunk_size(b"1a\r\n"), Some(26));
        assert_eq!(chunk_size(b"0000000000000000001A;name=value \t\r\n"), Some(26));
        for bad in [
            &b"\r\n"[..],
            b"x\r\n",
            b"10000000000000000\r\n",
            b"1;a\nb\r\n",
            b"1 x\r\n",
            b"1",
        ] {
            assert_eq!(chunk_size(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        let body = b"5\r\nhello\r\n6;x=y\r\n world\r\n0\r\nTrailer: t\r\n\r\nNEXT";
        let mut framing = Framing::Chunked(Chunk::Size(Vec::new()));
        let mut data = Vec::new();
        let mut all = Vec::new();
        // A byte at a time, as segments may bring it.
        let mut took = 0;
        for b in body.chunks(1) {
            let (n, ended) = walk(&mut framing, b, 1 << 10, &mut |p| {
                match p {
                    Piece::Data(d) => {
                        data.extend_from_slice(d);
                        all.extend_from_slice(d);
                    }
                    Piece::Frame(f) => all.extend_from_slice(f),
                }
                Ok(())
            })
            .unwrap();
            took += n;
            if ended {
                break;
            }
        }
        assert_eq!(&body[..took], &all[..]);
        assert_eq!(took, body.len() - 4, "the next message was taken as this one's");
        assert_eq!(data, b"hello worldTrailer: t\r\n\r\n");
        let mut broken = Framing::Chunked(Chunk::Size(Vec::new()));
        assert!(walk(&mut broken, b"5\r\nhelloXY", 1 << 10, &mut |_| Ok(())).is_err());
    }

    /// The hop-by-hop fields go (RFC 9110 §7.6.1): each a connection option names, with the
    /// option, and Proxy-Connection; `close`, `keep-alive` and `upgrade` stay, as the injector
    /// applies them to both hops alike; every other byte of the head stays as it came.
    #[test]
    fn hop_by_hop_fields_go_and_the_rest_stays() {
        let head = b"GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, X-Trace, upgrade, te\r\nX-Trace: 1\r\nTE: trailers\r\nProxy-Connection: keep-alive\r\nUpgrade: websocket\r\n\r\n";
        let edits = hop_by_hop(head, &parsed(head));
        let edits: Vec<(Range<usize>, &[u8])> =
            edits.iter().map(|(r, v)| (r.clone(), v.as_slice())).collect();
        assert_eq!(
            String::from_utf8(rewrite(head, &edits).to_vec()).unwrap(),
            "GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, upgrade\r\nUpgrade: websocket\r\n\r\n"
        );
        let plain = b"GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive\r\n\r\n";
        assert!(
            hop_by_hop(plain, &parsed(plain)).is_empty(),
            "a head of no hop's fields was changed"
        );
    }

    /// An echo of a value is masked wherever it lies in a response's data, split across what
    /// came at once and across a chunked body's framing; its framing is never read for one; a
    /// value's beginning waits only while it may be one, and a message's end lets it go.
    #[test]
    fn an_echo_is_masked_across_pieces_and_framing() {
        let root = root("shards test root");
        let mut credentials = credentials(&root);
        assert!(matches!(
            credentials.leases.ask(CLAUDE, 7, Instant::now()),
            lease::Asked::Waiting
        ));
        lease_all(&mut credentials);
        let run = Run {
            credentials: &mut credentials,
            waiter: 7,
            now: Instant::now(),
            audit: Vec::new(),
        };
        let mut mask = Mask::default();
        mask.filled.push((CLAUDE.into(), 0));
        let mut out = Vec::new();
        let (a, b) = VALUE.split_at(10);
        let (b, c) = b.split_at(5);
        let mut push = |bytes: &[u8], data: bool, out: &mut Vec<u8>| {
            mask.push(bytes, data, &run, &mut |o| {
                out.extend_from_slice(o);
                Ok(())
            })
            .unwrap();
        };
        push(b"{\"key\":\"", true, &mut out);
        assert_eq!(out, b"{\"key\":\"", "bytes that begin no value waited");
        push(a, true, &mut out);
        push(b"\r\n1f\r\n", false, &mut out);
        push(b, true, &mut out);
        push(c, true, &mut out);
        push(b"\"}", true, &mut out);
        let want = [
            &b"{\"key\":\""[..],
            &mask()[..10],
            b"\r\n1f\r\n",
            &mask()[10..],
            b"\"}",
        ]
        .concat();
        assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&want));
        // A value's beginning at a message's end goes on there.
        out.clear();
        push(&VALUE[..7], true, &mut out);
        assert!(
            out.is_empty(),
            "a value's beginning went before it could be told from one"
        );
        mask.end_message(&mut |o| {
            out.extend_from_slice(o);
            Ok(())
        })
        .unwrap();
        assert_eq!(out, &VALUE[..7]);
    }
}
