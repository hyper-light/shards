//! The credential injector (D242; kiln plan 01a1d2b88be57304b972761e2a5c6781 step 3). A flow
//! whose TLS names a host a credential grant names has the guest's TLS ended here, under a leaf
//! the run's CA mints for that name as its ClientHello comes (as mitmproxy and gondolin mint
//! theirs, f34), and the host's made from here and verified as the registry's client verifies
//! (f20); both are TLS 1.3 under TLS_AES_256_GCM_SHA384 alone (f21). The guest's ClientHello is
//! held while the host's TLS is made offering what the guest offered of HTTP/1.1 and HTTP/2, and
//! the guest's accepted with the protocol the host chose (f41), so both hops speak one. Over
//! HTTP/2 the injector is an end of each hop (`H2`, http2.rs; step 7). Over HTTP/1.1 it
//! is a gateway (RFC 9110 §3.7): each request's head is read whole, its credential
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

use std::collections::{BTreeMap, VecDeque};
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
use crate::hpack;
use crate::http2::{self, Code, End, Scope};
use crate::lease::{self, Asked};
use crate::ring::Ring;
use crate::signin::{self, Cli};

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

/// The application protocols the injector speaks (RFC 7301): HTTP/1.1, and HTTP/2 over TLS
/// (RFC 9113 §3.2). A guest offering neither is refused at its handshake.
const HTTP_1_1: &[u8] = b"http/1.1";
const H2: &[u8] = b"h2";

/// The host's side of an injected flow's TLS (D242): verified as the registry's client
/// verifies (D19, f20), offering, flow by flow, what the guest offered of the protocols the
/// injector speaks (HTTP/1.1 where it offered none), and resuming no session, as a session kept is
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

/// The guest's side for a flow to `name`'s host: the leaf the run's CA mints for it, `alpn`
/// alone (the protocol its host chose), no session kept and no ticket sent (R05).
fn guest_config(cert: Vec<u8>, key: Vec<u8>, alpn: &[u8]) -> Result<Arc<ServerConfig>, rustls::Error> {
    let mut config = ServerConfig::builder_with_provider(PROVIDER.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )?;
    config.alpn_protocols = vec![alpn.to_vec()];
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

/// What the injector did with a credential (D242, f26): toward which host, which credential
/// (a sign-in's name, for a sign-in's), what, and when; never a value or a placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub host: String,
    pub reference: String,
    pub what: Did,
    pub at: SystemTime,
}

/// What an [`Audit`] records (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Did {
    /// Its value filled in toward the host.
    Filled,
    /// A sign-in's token response from the host taken outside, its CLI given placeholders.
    Captured,
    /// A sign-in's refresh, asked of the host, answered outside.
    Refreshed,
    /// A sign-in's revocation, asked of the host, answered here, its stored tokens untouched.
    Revoked,
}

impl fmt::Display for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (reference, host) = (&self.reference, &self.host);
        match self.what {
            Did::Filled => write!(f, "shards-net: credential {reference} filled in toward {host}")?,
            Did::Captured => write!(
                f,
                "shards-net: sign-in {reference}: its tokens from {host} taken outside"
            )?,
            Did::Refreshed => write!(
                f,
                "shards-net: sign-in {reference}: its refresh at {host} answered outside"
            )?,
            Did::Revoked => write!(
                f,
                "shards-net: sign-in {reference}: its revocation at {host} answered here, its tokens kept"
            )?,
        }
        match self.at.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(t) => write!(f, " at {}.{:09} (Unix time)", t.as_secs(), t.subsec_nanos()),
            Err(_) => f.write_str(", the clock before the Unix epoch"),
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
            State::Opening(_) => "Flow(Opening)",
            State::Ended(_) => "Flow(Ended)",
            State::H2(_) => "Flow(H2)",
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
    /// A granted host's, its ClientHello held while the host's TLS is made (f41).
    Opening(Box<Opening>),
    /// A granted host's over HTTP/1.1: the guest's TLS ended here, the host's made from here.
    Ended(Box<Ended>),
    /// A granted host's over HTTP/2 (RFC 9113), both hops: the injector an end of each.
    H2(Box<H2>),
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
            State::Opening(o) => o.early.len(),
            State::Ended(e) => e.held(),
            State::H2(h) => h.held(),
            State::Gone => 0,
        }
    }

    /// The guest's bytes, in order.
    pub fn from_guest(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.hello(bytes, run, q),
            State::Opening(o) => {
                o.early.extend_from_slice(bytes);
                Said::Going
            }
            State::Ended(e) => e.guest_bytes(bytes, run, q),
            State::H2(h) => h.guest_bytes(bytes, run, q),
            State::Gone => Said::Going,
        }
    }

    /// The host's bytes, in order. Before the guest's ClientHello is whole, the host spoke
    /// first, which no TLS server does: the flow passes, these bytes to the guest as they came.
    pub fn from_host(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.pass(),
            State::Opening(o) => match o.host_bytes(bytes, q) {
                Ok(false) => Said::Going,
                Ok(true) => self.open(None, run, q),
                Err(why) => self.open(Some(why), run, q),
            },
            State::Ended(e) => e.host_bytes(bytes, run, q),
            State::H2(h) => h.host_bytes(bytes, run, q),
            State::Gone => Said::Going,
        }
    }

    /// The guest has ended its side (its FIN).
    pub fn guest_ended(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.pass(),
            State::Opening(o) => {
                o.host.send_close_notify();
                match send(&mut o.host, q.to_host) {
                    Ok(()) => Said::Close,
                    Err(_) => Said::Reset,
                }
            }
            State::Ended(e) => e.guest_ended(run, q),
            State::H2(h) => h.close(q),
            State::Gone => Said::Going,
        }
    }

    /// The host has ended its side (its socket's end).
    pub fn host_ended(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Hello { .. } => self.pass(),
            State::Opening(_) => self.open(Some("the host ended before its TLS was made".into()), run, q),
            State::Ended(e) => e.host_ended(run, q),
            State::H2(h) => h.host_failed("the host ended".into(), run, q),
            State::Gone => Said::Going,
        }
    }

    /// A lease the flow waited on was answered: its request goes on, or is refused.
    pub fn resume(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        match &mut self.state {
            State::Ended(e) => e.serve(run, q),
            State::H2(h) => h.serve(run, q),
            State::Hello { .. } | State::Opening(_) | State::Gone => Said::Going,
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
        // A grant's host, or a sign-in's: its token host, or a host its access token goes to,
        // which a capture during the run may grant (D242, step 8).
        let named = |toward: &[String]| toward.iter().any(|h| h.eq_ignore_ascii_case(&sni));
        let granted =
            run.credentials.grants.iter().any(|g| named(&g.toward))
                || run.credentials.sign_ins.grants.iter().any(|s| {
                    named(&s.toward) || signin::profile(s.cli).token.host.eq_ignore_ascii_case(&sni)
                });
        if !granted {
            return self.pass();
        }
        let offered = accepted
            .client_hello()
            .alpn()
            .map(|a| a.map(<[u8]>::to_vec).collect::<Vec<_>>());
        match Opening::new(&sni, accepted, rest, offered, run, q) {
            Ok(opening) => {
                self.state = State::Opening(Box::new(opening));
                Said::Going
            }
            Err(said) => {
                self.state = State::Gone;
                said
            }
        }
    }

    /// The host's TLS made, or failed: the guest's accepted speaking the protocol the host chose
    /// (HTTP/1.1 where it chose none, RFC 7301 §3.2), or, where the host failed, the guest's own
    /// first choice, so that its requests are answered 502 in it.
    fn open(&mut self, failed: Option<String>, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        let State::Opening(opening) = std::mem::replace(&mut self.state, State::Gone) else {
            return Said::Going;
        };
        let Opening {
            sni,
            accepted,
            early,
            mut host,
            offered,
        } = *opening;
        let h2 = match &failed {
            None => host.alpn_protocol() == Some(H2),
            Some(why) => {
                say(&sni, format_args!("the host's TLS failed: {why}"));
                offered.first().is_some_and(|p| p == H2)
            }
        };
        let guest = match accept(&sni, accepted, if h2 { H2 } else { HTTP_1_1 }, run, q) {
            Ok(guest) => guest,
            Err(said) => {
                host.send_close_notify();
                let _ = send(&mut host, q.to_host);
                return said;
            }
        };
        if h2 {
            let mut e = H2::new(sni, guest, host, failed);
            if let Err(said) = e.take_guest(&early, q) {
                return said;
            }
            self.state = State::H2(Box::new(e));
        } else {
            let sign_in = run.credentials.sign_ins.at_token_host(&sni);
            let mut e = Ended::new(sni, guest, host, sign_in, failed);
            if let Err(said) = e.take_guest(&early, q) {
                return said;
            }
            self.state = State::Ended(Box::new(e));
        }
        self.resume(run, q)
    }
}

/// The guest's TLS accepted under a leaf for `sni`, speaking `alpn`; refused with its alert.
fn accept(
    sni: &str,
    accepted: rustls::server::Accepted,
    alpn: &[u8],
    run: &mut Run<'_>,
    q: &mut Queues<'_>,
) -> Result<ServerConnection, Said> {
    let (cert, key) = run.credentials.ca.leaf(sni).map_err(|e| {
        say(sni, format_args!("no leaf: {e}"));
        Said::Reset
    })?;
    let config = guest_config(cert, key, alpn).map_err(|e| {
        say(sni, format_args!("no TLS of the guest's: {e}"));
        Said::Reset
    })?;
    match accepted.into_connection(config) {
        Ok(mut guest) => {
            // Its queues are the connection's, whose window bounds them; rustls's own bound
            // would only hold back what the window already lets come.
            guest.set_buffer_limit(None);
            Ok(guest)
        }
        Err((e, mut alert)) => {
            say(sni, format_args!("the guest's TLS refused: {e}"));
            Err(match alert.write_all(&mut Onto(&mut *q.to_guest)) {
                Ok(()) => Said::Close,
                Err(_) => Said::Reset,
            })
        }
    }
}

/// A granted host's flow while the host's TLS is made (D242, f41): the guest's ClientHello held,
/// and the guest's bytes after it, until the host has chosen among the protocols the guest
/// offered. A request waits for the host's TLS in any case, so holding the guest's costs it none.
struct Opening {
    sni: String,
    accepted: rustls::server::Accepted,
    early: Vec<u8>,
    host: ClientConnection,
    /// What the guest offered of what the injector speaks, in its order of preference.
    offered: Vec<Vec<u8>>,
}

impl Opening {
    /// The host's TLS begun, offering what the guest offered (`offered`, none where it sent no
    /// ALPN) of what the injector speaks; a sign-in's token host HTTP/1.1 alone, as its tokens are
    /// read from HTTP/1.1's messages (step 8). A guest offering none of it is refused at its
    /// handshake (RFC 7301 §3.2), no host's TLS begun.
    fn new(
        sni: &str,
        accepted: rustls::server::Accepted,
        rest: &[u8],
        offered: Option<Vec<Vec<u8>>>,
        run: &mut Run<'_>,
        q: &mut Queues<'_>,
    ) -> Result<Opening, Said> {
        let token_host = run.credentials.sign_ins.at_token_host(sni).is_some();
        let offered: Vec<Vec<u8>> = match offered {
            None => vec![HTTP_1_1.to_vec()],
            Some(all) => all
                .into_iter()
                .filter(|p| p == HTTP_1_1 || (p == H2 && !token_host))
                .collect(),
        };
        // A guest whose ClientHello says plainly that this side cannot serve it, no protocol or no
        // suite in common (RFC 7301 §3.2, RFC 8446 §4.1.1), is refused at once, no host's TLS
        // begun for it.
        let suites = accepted.client_hello().cipher_suites().to_vec();
        let suite = PROVIDER.cipher_suites.iter().any(|s| suites.contains(&s.suite()));
        if offered.is_empty() || !suite {
            return Err(match accept(sni, accepted, HTTP_1_1, run, q) {
                Ok(_) => Said::Reset,
                Err(said) => said,
            });
        }
        let name = ServerName::try_from(sni.to_string()).map_err(|_| Said::Reset)?;
        let config = run.credentials.upstream.0.clone();
        let mut host = ClientConnection::new_with_alpn(config, name, offered.clone()).map_err(|e| {
            say(sni, format_args!("no TLS of the host's: {e}"));
            Said::Reset
        })?;
        host.set_buffer_limit(None);
        send(&mut host, q.to_host).map_err(|_| Said::Reset)?;
        Ok(Opening {
            sni: sni.to_string(),
            accepted,
            early: rest.to_vec(),
            host,
            offered,
        })
    }

    /// The host's TLS bytes: whether its handshake is done, or why it failed.
    fn host_bytes(&mut self, bytes: &[u8], q: &mut Queues<'_>) -> Result<bool, String> {
        let mut rest = bytes;
        while !rest.is_empty() {
            match self.host.read_tls(&mut rest) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
            if let Err(e) = self.host.process_new_packets() {
                let _ = send(&mut self.host, q.to_host);
                return Err(e.to_string());
            }
        }
        send(&mut self.host, q.to_host).map_err(|e| e.to_string())?;
        Ok(!self.host.is_handshaking())
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
    /// A sign-in's access token toward an endpoint of its host that mints a credential its
    /// response carries in the clear (`signin::Profile::mints`): 403, never sent.
    Mints { host: String },
    /// A sign-in's token response that could not be read or written: 502, never passed on.
    SignIn(signin::Refusal),
    /// A sign-in's capture or refresh not answered outside: a refresh its broker refused is
    /// `invalid_grant` (400, RFC 6749 §5.2), so that its CLI signs in anew; the rest 502 for a
    /// capture, 503 for a refresh.
    Declined { refresh: bool, why: signin::Declined },
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
            Refusal::Mints { host } => write!(
                f,
                "a sign-in's token is never sent to {host}'s endpoint that mints a key in the clear"
            ),
            Refusal::SignIn(why) => write!(f, "{why}"),
            Refusal::Declined { refresh: true, why } => {
                write!(f, "the sign-in's refresh was not answered: {why}")
            }
            Refusal::Declined { refresh: false, why } => {
                write!(f, "the sign-in was not taken outside: {why}")
            }
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
            Refusal::Upstream(_) | Refusal::SignIn(_) => (502, "Bad Gateway"),
            Refusal::Mints { .. } => (403, "Forbidden"),
            Refusal::Declined {
                refresh: true,
                why: signin::Declined::Denied(_),
            } => (400, "Bad Request"),
            Refusal::Declined { refresh: true, .. } => (503, "Service Unavailable"),
            Refusal::Declined { refresh: false, .. } => (502, "Bad Gateway"),
        }
    }

    /// The response the guest gets, the connection closing after it.
    fn response(&self) -> Vec<u8> {
        let (status, reason) = self.status();
        if status == 400 && matches!(self, Refusal::Declined { .. }) {
            // RFC 6749 §5.2's error, which a CLI reads as its sign-in's end (its broker's words
            // go to the diagnostics, never into the guest).
            let body = r#"{"error":"invalid_grant"}"#;
            return format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes();
        }
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
    /// A sign-in's refresh, read whole and asked outside (D242, step 8): its answer resumes it.
    Refreshing,
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
    /// Where it is a sign-in's token host's POST to its token or revocation path: its body is
    /// read whole before it goes, as a refresh or a revocation is answered outside.
    whole: Option<Whole>,
}

/// What a sign-in's request read whole is asked by (D242, step 8): its CLI, method, target and
/// Content-Type.
struct Whole {
    cli: Cli,
    method: String,
    target: String,
    content_type: Option<Vec<u8>>,
}

/// Where the host's responses stand (R04).
enum Response {
    /// A response's head coming.
    Head(Vec<u8>),
    /// Its body coming to the guest as it comes, as framed.
    Body(Framing),
    /// Upgraded or tunnelled: the host's bytes go to the guest as they come.
    Tunnel,
    /// A sign-in's token host's response, its body read whole as it comes (D242, step 8), none of
    /// it to the guest before: its tokens taken outside, or it passed on as it came.
    Whole {
        head: Vec<u8>,
        body: Zeroizing<Vec<u8>>,
        framing: Framing,
    },
    /// Its tokens asked of the run host to keep, their placeholders coming in the answer.
    Capturing {
        head: Vec<u8>,
        body: Zeroizing<Vec<u8>>,
        tokens: signin::Tokens,
    },
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
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) line: Range<usize>,
    pub(crate) value: Range<usize>,
}

/// Where `part`, a slice of `buf`, lies in it.
fn place(buf: &[u8], part: &[u8]) -> Option<Range<usize>> {
    let start = (part.as_ptr() as usize).checked_sub(buf.as_ptr() as usize)?;
    let end = start.checked_add(part.len())?;
    (end <= buf.len()).then_some(start..end)
}

/// The fields httparse read of `head`, where each lies: each line from its name to its CRLF.
pub(crate) fn fields(head: &[u8], read: &[httparse::Header<'_>]) -> Option<Vec<Field>> {
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
pub(crate) fn values<'h>(
    head: &'h [u8],
    fields: &'h [Field],
    name: &'h str,
) -> impl Iterator<Item = &'h [u8]> + 'h {
    fields
        .iter()
        .filter(move |f| f.name == name)
        .map(|f| head.get(f.value.clone()).unwrap_or_default())
}

/// A comma-separated list's elements (RFC 9110 §5.6.1), trimmed, the empty ones dropped.
pub(crate) fn list(value: &[u8]) -> impl Iterator<Item = &[u8]> {
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

/// Where a credential's token lies in a field's value: an `Authorization` field's bearer token
/// (RFC 6750 §2.1, its scheme of any case, RFC 9110 §11.1) or an `x-api-key` field's whole
/// value, the two fields the harnesses send theirs in (f56, f65). `name` lowered, as HTTP/1.1's
/// fields are read and HTTP/2's are sent (RFC 9113 §8.2.1).
pub(crate) fn token_in(name: &[u8], value: &[u8]) -> Option<Range<usize>> {
    match name {
        b"authorization" => {
            let space = value.iter().position(|&b| b == b' ')?;
            let (scheme, rest) = value.split_at_checked(space)?;
            if !scheme.eq_ignore_ascii_case(b"bearer") {
                return None;
            }
            let start = space + rest.iter().take_while(|&&b| b == b' ').count();
            (start < value.len() && !value.get(start..)?.contains(&b' ')).then_some(start..value.len())
        }
        b"x-api-key" => (!value.is_empty()).then_some(0..value.len()),
        _ => None,
    }
}

/// A head's field's credential token, where it lies in the head.
fn token(head: &[u8], field: &Field) -> Option<Range<usize>> {
    let r = token_in(field.name.as_bytes(), head.get(field.value.clone())?)?;
    Some(field.value.start + r.start..field.value.start + r.end)
}

/// A sign-in's request made to ask its response as its sender wrote it (RFC 9110 §12.5.3:
/// `identity`), so that its tokens can be read where they lie: each Accept-Encoding line cut and
/// one asking `identity` written before the head's end (D242, step 8).
fn identity(head: &[u8], fields: &[Field]) -> Vec<(Range<usize>, Vec<u8>)> {
    let end = head.len().saturating_sub(2);
    fields
        .iter()
        .filter(|f| f.name == "accept-encoding")
        .map(|f| (f.line.clone(), Vec::new()))
        .chain([(end..end, b"Accept-Encoding: identity\r\n".to_vec())])
        .collect()
}

/// Whether `reference` is a sign-in's access token, and `host`'s `target` an endpoint its CLI's
/// profile names as minting a credential in the clear (`signin::Profile::mints`).
fn mints(run: &Run<'_>, reference: &str, host: &str, target: &str) -> bool {
    run.credentials.sign_ins.grants.iter().any(|s| {
        s.captured.as_ref().is_some_and(|c| c.access == reference)
            && signin::profile(s.cli).mints(host, target)
    })
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
    authority_host(authority).ok_or(Refusal::Malformed("a request's host is empty"))
}

/// An authority's host (RFC 3986 §3.2.2): without its user information, then its port, an IPv6
/// literal kept whole with its brackets; lowered, as `decide` compares hosts. None where nothing
/// is left.
pub(crate) fn authority_host(authority: &str) -> Option<String> {
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let name = match authority.find(']') {
        Some(close) if authority.starts_with('[') => authority.get(..=close).unwrap_or(authority),
        _ => authority
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(authority, |(name, _)| name),
    };
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
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
pub(crate) fn failure(v: &[u8]) -> Vec<usize> {
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
pub(crate) fn step(v: &[u8], table: &[usize], mut m: usize, b: u8) -> usize {
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

/// What a sign-in's request read whole comes to (R04).
enum Outside {
    /// Its body has not come whole.
    Waiting(Whole),
    /// Answered here or asked outside: it goes no further.
    Answered,
    /// Neither: it goes on as any request.
    Pass,
}

/// A sign-in's placeholders, as its capture gave them.
fn placeholders_of(c: &signin::Captured) -> signin::Placeholders {
    signin::Placeholders {
        access: c.access_placeholder.clone(),
        refresh: c.refresh_placeholder.clone(),
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
    /// Whose sign-in's token host this is, where it is one (D242, step 8): no value is filled
    /// toward it, and its responses are read whole for tokens.
    sign_in: Option<Cli>,
    /// What its response's side refuses, answered once that side's bytes are taken.
    refused: Option<Refusal>,
    /// The host ended its side while a sign-in's tokens were asked outside: the guest's ends
    /// once their answer has gone.
    host_closed: bool,
}

impl Ended {
    /// Its guest's TLS accepted under a leaf for `sni` and its host's made (`failed`, where it
    /// failed); `sign_in` the sign-in whose token host it is, where it is one.
    fn new(
        sni: String,
        guest: ServerConnection,
        host: ClientConnection,
        sign_in: Option<Cli>,
        failed: Option<String>,
    ) -> Ended {
        Ended {
            sni,
            guest,
            host,
            pending: Vec::new(),
            request: Request::Head,
            response: Response::Head(Vec::new()),
            owed: None,
            answered: false,
            mask: Mask::default(),
            failed,
            sign_in,
            refused: None,
            host_closed: false,
        }
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

    fn guest_bytes(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
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
    fn host_bytes(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
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
            if let Some(refusal) = self.refused.take() {
                return self.refuse(refusal, q);
            }
        }
        if closed {
            return self.host_done(run, q);
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
        self.host_done(run, q)
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
    fn host_done(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        // A sign-in's response read to the host's end ends there (D242, step 8); its tokens asked
        // outside, the guest's side ends once their answer has gone.
        if matches!(
            self.response,
            Response::Whole {
                framing: Framing::Close,
                ..
            }
        ) {
            self.finish_whole(run);
        }
        if let Some(refusal) = self.refused.take() {
            return self.refuse(refusal, q);
        }
        if matches!(self.response, Response::Capturing { .. }) {
            self.host_closed = true;
            return Said::Going;
        }
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
        if matches!(self.response, Response::Capturing { .. })
            && let Some(answer) = run.credentials.sign_ins.take(run.waiter)
        {
            if let Err(refusal) = self.captured(answer, run) {
                return self.refuse(refusal, q);
            }
            if self.host_closed {
                return self.host_done(run, q);
            }
        }
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
            Request::Refreshing => self.refreshed(run),
            Request::Done => {
                self.pending.clear();
                Ok(false)
            }
        }
    }

    /// A sign-in's request read whole (D242, step 8), once its body has come within the
    /// connection's window: a refresh of the sign-in's placeholder asked outside, its answer
    /// resuming the flow; a revocation of one answered here, its stored tokens untouched; anything
    /// else passed on, its response read. An answered request's body goes no further.
    fn outside(&mut self, parked: &Parked, whole: Whole, run: &mut Run<'_>) -> Result<Outside, Refusal> {
        let Framing::Length(n) = parked.body else {
            return Ok(Outside::Pass);
        };
        let n = usize::try_from(n).unwrap_or(usize::MAX);
        if self.pending.len() < n {
            return Ok(Outside::Waiting(whole));
        }
        let sign_ins = &mut run.credentials.sign_ins;
        let Some(index) = sign_ins.of(whole.cli) else {
            return Ok(Outside::Pass);
        };
        let placeholders = sign_ins
            .grants
            .get(index)
            .and_then(|g| g.captured.as_ref())
            .map(placeholders_of);
        let asked = signin::ask(
            signin::profile(whole.cli),
            &signin::Request {
                host: &parked.host,
                method: &whole.method,
                target: &whole.target,
                content_type: whole.content_type.as_deref(),
                body: self.pending.get(..n).unwrap_or_default(),
            },
            &placeholders.iter().collect::<Vec<_>>(),
        );
        match asked {
            signin::Ask::Pass => return Ok(Outside::Pass),
            signin::Ask::Refresh(_) => {
                sign_ins
                    .refresh(index, run.waiter)
                    .map_err(|why| Refusal::Declined { refresh: true, why })?;
                self.request = Request::Refreshing;
            }
            signin::Ask::Revoke(_) => {
                sign_ins.revoke(index);
                let sign_in = sign_ins
                    .grants
                    .get(index)
                    .map(|g| g.sign_in.clone())
                    .unwrap_or_default();
                self.guest
                    .writer()
                    .write_all(signin::REVOKED)
                    .map_err(|e| Refusal::Upstream(e.to_string()))?;
                run.audit.push(Audit {
                    host: parked.host.clone(),
                    reference: sign_in,
                    what: Did::Revoked,
                    at: SystemTime::now(),
                });
                self.request = Request::Head;
            }
        }
        self.pending.drain(..n);
        Ok(Outside::Answered)
    }

    /// A refresh asked outside (D242, step 8), once its answer has come: the token response its
    /// CLI is to have, its tokens the sign-in's placeholders and its ID token inert, kept from
    /// caches; or refused.
    fn refreshed(&mut self, run: &mut Run<'_>) -> Result<bool, Refusal> {
        let body = match run.credentials.sign_ins.take(run.waiter) {
            Some(signin::Answer::Refreshed(body)) => body,
            Some(signin::Answer::Declined(why)) => return Err(Refusal::Declined { refresh: true, why }),
            Some(signin::Answer::Captured) | None => {
                self.request = Request::Refreshing;
                return Ok(false);
            }
        };
        let sign_ins = &run.credentials.sign_ins;
        let grant = self
            .sign_in
            .and_then(|cli| sign_ins.of(cli))
            .and_then(|i| sign_ins.grants.get(i));
        let Some((sign_in, placeholders)) =
            grant.and_then(|g| Some((g.sign_in.clone(), placeholders_of(g.captured.as_ref()?))))
        else {
            return Err(Refusal::SignIn(signin::Refusal::NoPlaceholder));
        };
        let tokens = signin::read(&body)
            .map_err(Refusal::SignIn)?
            .ok_or(Refusal::SignIn(signin::Refusal::NoToken))?;
        let written = signin::write(&body, &tokens, &placeholders).map_err(Refusal::SignIn)?;
        self.guest
            .writer()
            .write_all(&signin::refreshed(&written))
            .map_err(|e| Refusal::Upstream(e.to_string()))?;
        run.audit.push(Audit {
            host: self.sni.clone(),
            reference: sign_in,
            what: Did::Refreshed,
            at: SystemTime::now(),
        });
        self.request = Request::Head;
        Ok(true)
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
        let mut edits = hop_by_hop(&head, &fields);
        let mut fills = Vec::new();
        let whole = match self.sign_in {
            // A sign-in's token host (D242, step 8): no value goes to it; its response is asked
            // as its sender writes it, so that its tokens can be read; a POST to its token or
            // revocation path is read whole, as a refresh or a revocation is answered outside.
            Some(cli) => {
                edits.extend(identity(&head, &fields));
                edits.sort_by_key(|(r, _)| r.start);
                let profile = signin::profile(cli);
                let path = signin::path(&target);
                let ours = method.eq_ignore_ascii_case("POST")
                    && (path == profile.token.path || path == profile.revoke.path);
                let fits = matches!(body, Framing::Length(n) if usize::try_from(n).is_ok_and(|n| n <= crate::tcp::TO_HOST));
                (ours && fits).then(|| Whole {
                    cli,
                    method: method.clone(),
                    target: target.clone(),
                    content_type: values(&head, &fields, "content-type").next().map(<[u8]>::to_vec),
                })
            }
            None => {
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
                    let Some(token) = head.get(range.clone()).and_then(|t| std::str::from_utf8(t).ok())
                    else {
                        continue;
                    };
                    if let Fill::With(reference) = credential::decide(grants, &self.sni, &host, Some(token)) {
                        if mints(run, &reference, &host, &target) {
                            return Err(Refusal::Mints { host });
                        }
                        fills.push((range, reference));
                    }
                }
                None
            }
        };
        Ok(Some(Parked {
            head,
            edits,
            fills,
            host,
            body,
            owed,
            whole,
        }))
    }

    /// A parked request goes once the host's TLS is up and each value it carries is leased:
    /// its head as it came but for its hop-by-hop fields and its placeholders, each filled in
    /// and audited; refused where a lease is; parked again while one is asked.
    fn go(&mut self, mut parked: Box<Parked>, run: &mut Run<'_>) -> Result<bool, Refusal> {
        if let Some(whole) = parked.whole.take() {
            match self.outside(&parked, whole, run)? {
                Outside::Waiting(whole) => {
                    parked.whole = Some(whole);
                    self.request = Request::Parked(parked);
                    return Ok(false);
                }
                Outside::Answered => return Ok(true),
                Outside::Pass => {}
            }
        }
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
                    what: Did::Filled,
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
    fn respond(&mut self, mut bytes: &[u8], run: &mut Run<'_>) -> Result<(), Broken> {
        let queued = |_: io::Error| Broken("its bytes could not be queued");
        while !bytes.is_empty() {
            if matches!(self.request, Request::Done) || self.refused.is_some() {
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
                Response::Whole { body, framing, .. } => {
                    let (taken, ended) = walk(framing, bytes, crate::tcp::TO_GUEST, &mut |p| {
                        if let Piece::Data(data) = p {
                            if body.len().saturating_add(data.len()) > crate::tcp::TO_GUEST {
                                return Err(io::Error::other("past what its connection holds"));
                            }
                            signin::append(body, data);
                        }
                        Ok(())
                    })?;
                    bytes = bytes.get(taken..).unwrap_or_default();
                    if ended {
                        self.finish_whole(run);
                    }
                }
                Response::Capturing { .. } => {
                    return Err(Broken("bytes past a sign-in's response, one request at a time"));
                }
            }
        }
        Ok(())
    }

    /// A sign-in's token host's response, whole (D242, step 8), read for tokens: one with none
    /// goes to the guest as it came, framed by its length; one with tokens waits while they are
    /// asked outside; one that cannot be read is refused, none of it passed on.
    fn finish_whole(&mut self, run: &mut Run<'_>) {
        let Response::Whole { head, body, .. } =
            std::mem::replace(&mut self.response, Response::Head(Vec::new()))
        else {
            return;
        };
        let read = signin::read(&body).map_err(Refusal::SignIn);
        let went = read.and_then(|tokens| match tokens {
            None => self.pass_whole(&head, &body),
            Some(tokens) => {
                let sign_ins = &mut run.credentials.sign_ins;
                let index = self.sign_in.and_then(|cli| sign_ins.of(cli));
                let (Some(index), Some(taken)) = (index, signin::Taken::of(&tokens)) else {
                    return Err(Refusal::SignIn(signin::Refusal::NoToken));
                };
                sign_ins
                    .capture(index, taken, run.waiter)
                    .map_err(|why| Refusal::Declined { refresh: false, why })?;
                self.response = Response::Capturing { head, body, tokens };
                Ok(())
            }
        });
        if let Err(refusal) = went {
            self.refused = Some(refusal);
        }
    }

    /// A sign-in's response with no token in it, to the guest as it came, framed by its length.
    fn pass_whole(&mut self, head: &[u8], body: &[u8]) -> Result<(), Refusal> {
        let framed = signin::frame(head, body.len()).map_err(Refusal::SignIn)?;
        let mut writer = self.guest.writer();
        writer
            .write_all(&framed)
            .and_then(|()| writer.write_all(body))
            .map_err(|e| Refusal::Upstream(e.to_string()))?;
        self.answered = true;
        self.exchange_ended();
        Ok(())
    }

    /// A capture's answer (D242, step 8): the token response goes to the guest with the
    /// sign-in's placeholders in its tokens' places and its ID token inert, framed anew; its real
    /// tokens wiped as they drop here; or it is refused.
    fn captured(&mut self, answer: signin::Answer, run: &mut Run<'_>) -> Result<(), Refusal> {
        let Response::Capturing { head, body, tokens } =
            std::mem::replace(&mut self.response, Response::Head(Vec::new()))
        else {
            return Ok(());
        };
        match answer {
            signin::Answer::Captured => {}
            signin::Answer::Declined(why) => return Err(Refusal::Declined { refresh: false, why }),
            signin::Answer::Refreshed(_) => {
                self.response = Response::Capturing { head, body, tokens };
                return Ok(());
            }
        }
        let sign_ins = &run.credentials.sign_ins;
        let grant = self
            .sign_in
            .and_then(|cli| sign_ins.of(cli))
            .and_then(|i| sign_ins.grants.get(i));
        let Some((sign_in, placeholders)) =
            grant.and_then(|g| Some((g.sign_in.clone(), placeholders_of(g.captured.as_ref()?))))
        else {
            return Err(Refusal::SignIn(signin::Refusal::NoPlaceholder));
        };
        let written = signin::write(&body, &tokens, &placeholders).map_err(Refusal::SignIn)?;
        let framed = signin::frame(&head, written.len()).map_err(Refusal::SignIn)?;
        let mut writer = self.guest.writer();
        writer
            .write_all(&framed)
            .and_then(|()| writer.write_all(&written))
            .map_err(|e| Refusal::Upstream(e.to_string()))?;
        self.answered = true;
        self.exchange_ended();
        run.audit.push(Audit {
            host: self.sni.clone(),
            reference: sign_in,
            what: Did::Captured,
            at: SystemTime::now(),
        });
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
        // A sign-in's token host's final response with a body is read whole before any of it
        // goes, so that its tokens are taken outside (D242, step 8).
        if self.sign_in.is_some() && !tunnel && !interim && framing != Framing::Length(0) {
            if let Err(why) = signin::readable(&passed) {
                self.refused = Some(Refusal::SignIn(why));
                return Ok(());
            }
            self.response = Response::Whole {
                head: passed.to_vec(),
                body: Zeroizing::new(Vec::new()),
                framing,
            };
            return Ok(());
        }
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

/// Where one hop of an HTTP/2 flow stands (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The client preface's octets awaited (§3.4): the guest's alone.
    Preface,
    /// The peer's first frame awaited, which is its SETTINGS (§3.4).
    Settings,
    Open,
    /// A GOAWAY sent or received (§6.8): no new stream on it, and the flow ends once its
    /// streams have.
    Away,
}

/// A stream's state on one hop as RFC 9113 §5.1 has it, from the injector's end. Idle is a
/// stream not yet in the map (the guest's) or not yet sent (the host's); push's reserved states
/// never come, push being off (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

impl StreamState {
    /// This end sent END_STREAM.
    fn sent_end(self) -> StreamState {
        match self {
            StreamState::Open => StreamState::HalfClosedLocal,
            StreamState::HalfClosedRemote | StreamState::Closed => StreamState::Closed,
            StreamState::HalfClosedLocal => StreamState::HalfClosedLocal,
        }
    }

    /// The peer sent END_STREAM.
    fn received_end(self) -> StreamState {
        match self {
            StreamState::Open => StreamState::HalfClosedRemote,
            StreamState::HalfClosedLocal | StreamState::Closed => StreamState::Closed,
            StreamState::HalfClosedRemote => StreamState::HalfClosedRemote,
        }
    }

    /// The peer may still send on it.
    fn receiving(self) -> bool {
        matches!(self, StreamState::Open | StreamState::HalfClosedLocal)
    }

    /// This end may still send on it.
    fn sending(self) -> bool {
        matches!(self, StreamState::Open | StreamState::HalfClosedRemote)
    }
}

/// A request's way to the host (R04).
enum Upward {
    /// Decoded, waiting for its leases or for room among the host's streams
    /// (SETTINGS_MAX_CONCURRENT_STREAMS, §5.1.2).
    Parked(Box<http2::Go>),
    /// Its HEADERS sent on the host's stream `id`, which stands at `state`.
    Sent { id: u32, state: StreamState },
    /// Answered here and never sent: what the guest sends on it is dropped.
    Answered,
}

/// What ends one direction's message once its DATA has gone (§8.1): END_STREAM, or a trailer
/// section, encoded as it goes, as each hop's blocks are decoded in the order they are sent
/// (§4.3).
enum Last {
    End,
    Trailers(Vec<hpack::Field>),
}

/// Where a stream's response stands (§8.1): its final head awaited, informational ones passed
/// on; or its body coming, after which a field block is its trailers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Down {
    Head,
    Body,
}

/// One of the guest's streams: its state on the guest's hop, its request's way to the host,
/// each direction's DATA held while the next hop's windows are spent, and the values filled into
/// its request, masked in its response.
struct Stream {
    guest: StreamState,
    up: Upward,
    up_data: Vec<u8>,
    up_last: Option<Last>,
    down: Down,
    down_data: Vec<u8>,
    down_last: Option<Last>,
    mask: Mask,
}

impl Stream {
    fn new(end_stream: bool) -> Stream {
        Stream {
            guest: if end_stream {
                StreamState::HalfClosedRemote
            } else {
                StreamState::Open
            },
            up: Upward::Answered,
            up_data: Vec::new(),
            up_last: end_stream.then_some(Last::End),
            down: Down::Head,
            down_data: Vec::new(),
            down_last: None,
            mask: Mask::default(),
        }
    }

    /// What it holds of the guest's: its request's fields while parked, its DATA and trailers.
    fn held(&self) -> usize {
        let fields = |f: &[hpack::Field]| f.iter().map(hpack::Field::size).sum::<usize>();
        let parked = match &self.up {
            Upward::Parked(go) => fields(&go.fields),
            _ => 0,
        };
        let trailers = match &self.up_last {
            Some(Last::Trailers(f)) => fields(f),
            _ => 0,
        };
        parked.saturating_add(trailers).saturating_add(self.up_data.len())
    }
}

/// Which hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Guest,
    Host,
}

/// A stream error (§5.4.2).
fn stream_error(stream: u32, code: Code, why: &'static str) -> http2::Error {
    http2::Error {
        scope: Scope::Stream(stream),
        code,
        why,
    }
}

/// A PING (§6.7): answered on its own hop.
fn ping(head: http2::Head, payload: &[u8], out: &mut Vec<u8>) -> Result<(), http2::Error> {
    if head.stream != 0 {
        return Err(http2::connection(Code::Protocol, "a PING on a stream"));
    }
    if payload.len() != 8 {
        return Err(http2::connection(Code::FrameSize, "a PING not of eight octets"));
    }
    if head.flags & http2::ACK == 0 {
        http2::ping_ack(payload, out);
    }
    Ok(())
}

/// `value`, a field's, its echoes of the values `mask` masks replaced.
fn masked(mask: &mut Mask, value: &[u8], run: &Run<'_>) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(value.len()));
    let _ = mask.push(value, true, run, &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    });
    let _ = mask.end_message(&mut |b| {
        out.extend_from_slice(b);
        Ok(())
    });
    out
}

/// What this side holds of a hop's peer's DATA (`http2::Holds`): `n`, what its connection holds
/// that way (tcp.rs' `TO_HOST`, `TO_GUEST`), within a window's 2^31-1 (§6.9.1).
fn holds(n: usize) -> http2::Holds {
    let n = u32::try_from(n).unwrap_or(u32::MAX).min(http2::MOST_STREAM);
    http2::Holds {
        stream: n,
        connection: n,
    }
}

/// A granted host's flow over HTTP/2 (RFC 9113; D242, step 7): the guest's TLS ended here and
/// the host's made from here, the injector an end of each hop, each with its own settings,
/// windows and field compression (§4.3.1, §6.9). Each request's block is decoded on the guest's
/// hop, filled in, and encoded anew on the host's; each response's decoded on the host's hop,
/// its echoes masked, and encoded anew on the guest's. Host stream ids are given in the order
/// requests go, so that a request parked on its lease holds back no other (§5.1.1).
struct H2 {
    sni: String,
    guest: ServerConnection,
    host: ClientConnection,
    guest_hop: http2::Hop,
    host_hop: http2::Hop,
    guest_phase: Phase,
    host_phase: Phase,
    /// Each side's plaintext not yet framed.
    from_guest: Vec<u8>,
    from_host: Zeroizing<Vec<u8>>,
    /// A field block coming from each, until its END_HEADERS (§6.10).
    guest_block: Option<http2::Block>,
    host_block: Option<http2::Block>,
    /// The guest's streams by its ids, and the last it opened (§5.1.1).
    streams: BTreeMap<u32, Stream>,
    last_guest: u32,
    /// The guest's stream each host stream carries, and the host stream id to be sent next.
    by_host: BTreeMap<u32, u32>,
    next_host: u32,
    /// Each hop's frames, to go.
    to_guest: Vec<u8>,
    to_host: Zeroizing<Vec<u8>>,
    /// Why the host's side failed: each request not yet sent is answered 502.
    failed: Option<String>,
}

impl H2 {
    fn new(sni: String, guest: ServerConnection, host: ClientConnection, failed: Option<String>) -> H2 {
        // What each decoder lets its peer's table be: the protocol's initial size, which this
        // side never changes (§6.5.2), so that no encoder's table is larger than the other hop's.
        let table = http2::Settings::INITIAL.header_table_size as usize;
        let mut h = H2 {
            sni,
            guest,
            host,
            guest_hop: http2::Hop::new(holds(crate::tcp::TO_HOST), table),
            host_hop: http2::Hop::new(holds(crate::tcp::TO_GUEST), table),
            guest_phase: Phase::Preface,
            host_phase: if failed.is_some() {
                Phase::Away
            } else {
                Phase::Settings
            },
            from_guest: Vec::new(),
            from_host: Zeroizing::new(Vec::new()),
            guest_block: None,
            host_block: None,
            streams: BTreeMap::new(),
            last_guest: 0,
            by_host: BTreeMap::new(),
            next_host: 1,
            to_guest: Vec::new(),
            to_host: Zeroizing::new(Vec::new()),
            failed,
        };
        h.guest_hop.preface(End::Server, &mut h.to_guest);
        if h.failed.is_none() {
            h.host_hop.preface(End::Client, &mut h.to_host);
        }
        h
    }

    /// What it holds of the guest's bytes: its plaintext not yet framed, a block coming, and
    /// each stream's.
    fn held(&self) -> usize {
        self.streams.values().fold(
            self.from_guest
                .len()
                .saturating_add(self.guest_block.as_ref().map_or(0, http2::Block::held)),
            |n, s| n.saturating_add(s.held()),
        )
    }

    /// The guest's TLS bytes into its connection, its plaintext into `from_guest`; its TLS's
    /// failure answered with its alert, the flow closing.
    fn take_guest(&mut self, bytes: &[u8], q: &mut Queues<'_>) -> Result<(), Said> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let read = self.guest.read_tls(&mut rest);
            let processed = read.as_ref().ok().map(|_| self.guest.process_new_packets());
            let why = match (read, processed) {
                (Ok(0), _) | (Ok(_), None) => break,
                (Ok(_), Some(Ok(_))) => None,
                (Err(e), _) => Some(e.to_string()),
                (Ok(_), Some(Err(e))) => Some(e.to_string()),
            };
            if let Some(why) = why {
                say(&self.sni, format_args!("the guest's TLS failed: {why}"));
                self.host.send_close_notify();
                return Err(
                    match (send(&mut self.guest, q.to_guest), send(&mut self.host, q.to_host)) {
                        (Ok(()), Ok(())) => Said::Close,
                        _ => Said::Reset,
                    },
                );
            }
            if plain(&mut self.guest, &mut self.from_guest) {
                // Its close_notify: it opens no stream more.
                self.guest_phase = Phase::Away;
            }
        }
        Ok(())
    }

    fn guest_bytes(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        if let Err(said) = self.take_guest(bytes, q) {
            return said;
        }
        self.serve(run, q)
    }

    /// The host's TLS bytes, its plaintext into `from_host`.
    fn host_bytes(&mut self, bytes: &[u8], run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        if self.failed.is_some() {
            return self.serve(run, q);
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let read = self.host.read_tls(&mut rest);
            let processed = read.as_ref().ok().map(|_| self.host.process_new_packets());
            match (read, processed) {
                (Ok(0), _) | (Ok(_), None) => break,
                (Ok(_), Some(Ok(_))) => {}
                (Err(e), _) => return self.host_failed(e.to_string(), run, q),
                (Ok(_), Some(Err(e))) => return self.host_failed(e.to_string(), run, q),
            }
            // Its plaintext taken as each record is, as rustls holds no more than its own bound;
            // its close_notify read once its frames before it are (`serve`).
            if plain(&mut self.host, &mut self.from_host) {
                break;
            }
        }
        self.serve(run, q)
    }

    /// The host's side has failed or ended: each stream whose response has not begun is
    /// answered 502, one whose response has is reset, and the guest told the connection goes
    /// (§6.8), as no request can reach the host.
    fn host_failed(&mut self, why: String, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        say(&self.sni, format_args!("the host's side ended: {why}"));
        let _ = send(&mut self.host, q.to_host);
        self.failed = Some(why.clone());
        self.host_phase = Phase::Away;
        let sent: Vec<(u32, Option<u32>, Down)> = self
            .streams
            .iter()
            .filter_map(|(&gid, s)| match s.up {
                Upward::Sent { id, state } => Some((gid, (!state.receiving()).then_some(id), s.down)),
                _ => None,
            })
            .collect();
        for (gid, ended, down) in sent {
            match (ended, down) {
                // Its response whole: it goes on to the guest, its request no further (§8.1).
                (Some(id), _) => {
                    self.by_host.remove(&id);
                    self.host_hop.close(id);
                    if let Some(s) = self.streams.get_mut(&gid) {
                        s.up = Upward::Answered;
                        self.guest_hop.released(gid, s.up_data.len(), &mut self.to_guest);
                        s.up_data.clear();
                        s.up_last = None;
                    }
                }
                (None, Down::Head) => self.answer(gid, Refusal::Upstream(why.clone())),
                (None, Down::Body) => self.reset(Which::Guest, gid, Code::Internal),
            }
        }
        self.go_away_guest(Code::NoError);
        self.serve(run, q)
    }

    /// Takes each side's frames, moves each stream as far as it goes, and sends what each hop
    /// is to have; the connection ends once a hop has gone away and its streams have ended.
    fn serve(&mut self, run: &mut Run<'_>, q: &mut Queues<'_>) -> Said {
        let closed = self.failed.is_none() && plain(&mut self.host, &mut self.from_host);
        if let Err((which, e)) = self
            .frames(Which::Guest, run)
            .and_then(|()| self.frames(Which::Host, run))
        {
            return self.fail(which, e, q);
        }
        if closed {
            return self.host_failed("the host ended its TLS".into(), run, q);
        }
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for gid in ids {
            self.go(gid, run);
            self.flush_up(gid);
            self.flush_down(gid);
            self.reap(gid);
        }
        if self.streams.is_empty() && self.guest_phase == Phase::Away {
            return self.close(q);
        }
        self.flush(q)
    }

    /// Each hop's frames to `to_guest` and `to_host`, and the TLS records each side is to have.
    fn flush(&mut self, q: &mut Queues<'_>) -> Said {
        let guest = self.guest.writer().write_all(&self.to_guest);
        self.to_guest.clear();
        let host = if self.failed.is_none() {
            self.host.writer().write_all(&self.to_host)
        } else {
            Ok(())
        };
        self.to_host.clear();
        match (
            guest,
            host,
            send(&mut self.guest, q.to_guest),
            send(&mut self.host, q.to_host),
        ) {
            (Ok(()), Ok(()), Ok(()), Ok(())) => Said::Going,
            _ => Said::Reset,
        }
    }

    /// Ends the flow: what each hop is to have, then each side's TLS.
    fn close(&mut self, q: &mut Queues<'_>) -> Said {
        if self.flush(q) == Said::Reset {
            return Said::Reset;
        }
        self.guest.send_close_notify();
        self.host.send_close_notify();
        match (send(&mut self.guest, q.to_guest), send(&mut self.host, q.to_host)) {
            (Ok(()), Ok(())) => Said::Close,
            _ => Said::Reset,
        }
    }

    /// A connection error (§5.4.1): a GOAWAY with its code to the hop that erred, the other told
    /// the connection goes, and the flow ends.
    fn fail(&mut self, which: Which, e: http2::Error, q: &mut Queues<'_>) -> Said {
        say(
            &self.sni,
            format_args!("the {which:?}'s HTTP/2 failed ({:?}): {}", e.code, e.why),
        );
        match which {
            Which::Guest => {
                http2::goaway(self.last_guest, e.code as u32, &mut self.to_guest);
                if self.failed.is_none() {
                    http2::goaway(0, Code::NoError as u32, &mut self.to_host);
                }
            }
            Which::Host => {
                http2::goaway(0, e.code as u32, &mut self.to_host);
                http2::goaway(self.last_guest, Code::Internal as u32, &mut self.to_guest);
            }
        }
        self.close(q)
    }

    /// Tells the guest the connection goes, its streams up to its last taken (§6.8).
    fn go_away_guest(&mut self, code: Code) {
        if self.guest_phase != Phase::Away {
            http2::goaway(self.last_guest, code as u32, &mut self.to_guest);
            self.guest_phase = Phase::Away;
        }
    }

    /// Every whole frame `which` sent; a stream error resets its stream, a connection error is
    /// the caller's.
    fn frames(&mut self, which: Which, run: &mut Run<'_>) -> Result<(), (Which, http2::Error)> {
        if which == Which::Host && self.failed.is_some() {
            self.from_host.clear();
            return Ok(());
        }
        if which == Which::Guest && self.guest_phase == Phase::Preface {
            let n = http2::PREFACE.len().min(self.from_guest.len());
            if self.from_guest.get(..n) != http2::PREFACE.get(..n) {
                return Err((
                    which,
                    http2::connection(Code::Protocol, "no HTTP/2 client preface"),
                ));
            }
            if n < http2::PREFACE.len() {
                return Ok(());
            }
            self.from_guest.drain(..n);
            self.guest_phase = Phase::Settings;
        }
        let most = http2::Settings::INITIAL.max_frame_size;
        let mut at = 0usize;
        loop {
            let buf: &[u8] = match which {
                Which::Guest => &self.from_guest,
                Which::Host => &self.from_host,
            };
            let rest = buf.get(at..).unwrap_or_default();
            let Some((head, payload)) = http2::frame(rest, most).map_err(|e| (which, e))? else {
                break;
            };
            let payload = Zeroizing::new(payload.to_vec());
            at = at.saturating_add(http2::HEAD).saturating_add(payload.len());
            let done = match which {
                Which::Guest => self.guest_frame(head, &payload, run),
                Which::Host => self.host_frame(head, &payload, run),
            };
            match done {
                Ok(()) => {}
                Err(http2::Error {
                    scope: Scope::Stream(id),
                    code,
                    why,
                }) => {
                    say(
                        &self.sni,
                        format_args!("{which:?} stream {id} reset ({code:?}): {why}"),
                    );
                    self.reset(which, id, code);
                }
                Err(e) => return Err((which, e)),
            }
        }
        let buf = match which {
            Which::Guest => &mut self.from_guest,
            Which::Host => &mut *self.from_host,
        };
        buf.drain(..at.min(buf.len()));
        Ok(())
    }

    /// A stream ended by an error on `which`'s hop (its id there): reset there with `code`, and
    /// on the other hop with CANCEL toward the host or INTERNAL_ERROR toward the guest.
    fn reset(&mut self, which: Which, id: u32, code: Code) {
        let gid = match which {
            Which::Guest => Some(id),
            Which::Host => self.by_host.get(&id).copied(),
        };
        let stream = gid.and_then(|gid| Some((gid, self.streams.remove(&gid)?)));
        if let Some((gid, s)) = stream {
            let (to_guest, to_host) = match which {
                Which::Guest => (code, Code::Cancel),
                Which::Host => (Code::Internal, code),
            };
            let host_open = matches!(s.up, Upward::Sent { state, .. } if state != StreamState::Closed);
            let host_id = match s.up {
                Upward::Sent { id, .. } => Some(id),
                _ => None,
            };
            self.forget(gid, &s);
            if s.guest != StreamState::Closed {
                http2::rst_stream(gid, to_guest as u32, &mut self.to_guest);
            }
            if let (true, Some(h)) = (host_open && self.failed.is_none(), host_id) {
                http2::rst_stream(h, to_host as u32, &mut self.to_host);
            }
        } else {
            let out = match which {
                Which::Guest => &mut self.to_guest,
                Which::Host => &mut *self.to_host,
            };
            http2::rst_stream(id, code as u32, out);
        }
    }

    /// A stream let go of: what it held given back to its peers' windows, and each hop's state
    /// of it dropped.
    fn forget(&mut self, gid: u32, s: &Stream) {
        self.guest_hop.released(gid, s.up_data.len(), &mut self.to_guest);
        self.guest_hop.close(gid);
        if let Upward::Sent { id, .. } = s.up {
            self.host_hop.released(id, s.down_data.len(), &mut self.to_host);
            self.host_hop.close(id);
            self.by_host.remove(&id);
        }
    }

    /// The guest's stream `gid` refused, never processed, which its client may retry (§8.7).
    fn refuse_stream(&mut self, gid: u32) {
        if let Some(s) = self.streams.remove(&gid) {
            self.forget(gid, &s);
        }
        http2::rst_stream(gid, Code::RefusedStream as u32, &mut self.to_guest);
    }

    /// A stream ended on both hops is let go of.
    fn reap(&mut self, gid: u32) {
        let done = self.streams.get(&gid).is_some_and(|s| {
            s.guest == StreamState::Closed
                && !matches!(s.up, Upward::Sent { state, .. } if state != StreamState::Closed)
        });
        if done && let Some(s) = self.streams.remove(&gid) {
            self.forget(gid, &s);
        }
    }

    fn guest_frame(
        &mut self,
        head: http2::Head,
        payload: &[u8],
        run: &mut Run<'_>,
    ) -> Result<(), http2::Error> {
        if let Some(mut block) = self.guest_block.take() {
            let most = crate::tcp::TO_HOST.saturating_sub(self.held());
            if block.continued(head, payload, most)? {
                return self.request_block(block, run);
            }
            self.guest_block = Some(block);
            return Ok(());
        }
        if self.guest_phase == Phase::Settings {
            if head.kind != http2::SETTINGS || head.flags & http2::ACK != 0 {
                return Err(http2::connection(
                    Code::Protocol,
                    "the guest's preface not followed by its SETTINGS",
                ));
            }
            self.guest_phase = Phase::Open;
        }
        match head.kind {
            http2::SETTINGS => self.guest_hop.settings(head, payload, &mut self.to_guest),
            http2::WINDOW_UPDATE => self.window_update(Which::Guest, head, payload),
            http2::PING => ping(head, payload, &mut self.to_guest),
            http2::HEADERS => {
                let (block, whole) = http2::Block::headers(head, payload)?;
                if whole {
                    return self.request_block(block, run);
                }
                self.guest_block = Some(block);
                Ok(())
            }
            http2::CONTINUATION => Err(http2::connection(Code::Protocol, "a CONTINUATION after no block")),
            http2::DATA => self.guest_data(head, payload),
            http2::RST_STREAM => self.guest_rst(head, payload),
            http2::GOAWAY => {
                if head.stream != 0 || payload.len() < 8 {
                    return Err(http2::connection(Code::Protocol, "a GOAWAY on a stream or short"));
                }
                self.guest_phase = Phase::Away;
                Ok(())
            }
            http2::PUSH_PROMISE => Err(http2::connection(
                Code::Protocol,
                "a PUSH_PROMISE from a client (§8.4)",
            )),
            // PRIORITY is deprecated (§5.3.2) and every other type unknown here (§5.5).
            _ => Ok(()),
        }
    }

    /// A WINDOW_UPDATE on `which`'s hop; one on a stream let go of is passed over, its window
    /// gone with it (§6.9).
    fn window_update(&mut self, which: Which, head: http2::Head, payload: &[u8]) -> Result<(), http2::Error> {
        let (hop, known) = match which {
            Which::Guest => (&mut self.guest_hop, self.streams.contains_key(&head.stream)),
            Which::Host => (&mut self.host_hop, self.by_host.contains_key(&head.stream)),
        };
        if head.stream != 0 && !known {
            return if payload.len() == 4 {
                Ok(())
            } else {
                Err(http2::connection(
                    Code::FrameSize,
                    "a WINDOW_UPDATE not of four octets",
                ))
            };
        }
        hop.window_update(head, payload)
    }

    fn guest_data(&mut self, head: http2::Head, payload: &[u8]) -> Result<(), http2::Error> {
        if head.stream == 0 {
            return Err(http2::connection(Code::Protocol, "DATA on stream 0"));
        }
        self.guest_hop.received(head)?;
        let data = http2::data(head, payload)?;
        let length = head.length as usize;
        let Some(s) = self.streams.get_mut(&head.stream) else {
            if head.stream > self.last_guest {
                return Err(http2::connection(Code::Protocol, "DATA on an idle stream"));
            }
            // A stream let go of here: its DATA dropped, its credit given back.
            self.guest_hop.released(head.stream, length, &mut self.to_guest);
            self.guest_hop.close(head.stream);
            return Ok(());
        };
        if !s.guest.receiving() {
            return Err(stream_error(
                head.stream,
                Code::StreamClosed,
                "DATA after its END_STREAM",
            ));
        }
        let end = head.flags & http2::END_STREAM != 0;
        if end {
            s.guest = s.guest.received_end();
        }
        let mut dropped = length.saturating_sub(data.len());
        if matches!(s.up, Upward::Answered) {
            dropped = length;
        } else {
            s.up_data.extend_from_slice(data);
            if end {
                s.up_last = Some(Last::End);
            }
        }
        self.guest_hop.released(head.stream, dropped, &mut self.to_guest);
        Ok(())
    }

    fn guest_rst(&mut self, head: http2::Head, payload: &[u8]) -> Result<(), http2::Error> {
        let (true, Some(code)) = (
            head.stream != 0,
            http2::word(payload, 0).filter(|_| payload.len() == 4),
        ) else {
            return Err(http2::connection(
                Code::FrameSize,
                "a RST_STREAM on stream 0 or not of four octets",
            ));
        };
        if head.stream > self.last_guest {
            return Err(http2::connection(
                Code::Protocol,
                "a RST_STREAM on an idle stream",
            ));
        }
        if let Some(s) = self.streams.remove(&head.stream) {
            self.forget(head.stream, &s);
            if let Upward::Sent { id, state } = s.up
                && state != StreamState::Closed
                && self.failed.is_none()
            {
                http2::rst_stream(id, code, &mut self.to_host);
            }
        }
        Ok(())
    }

    /// A request's block, or its trailers (§8.1): a new stream's request held to RFC 9113 and
    /// decided by `http2::request`, answered here where it is refused, parked otherwise; its
    /// decoded list past what the connection holds less what it holds already answered 431,
    /// the block decoded all the same (§10.5.1).
    fn request_block(&mut self, block: http2::Block, run: &mut Run<'_>) -> Result<(), http2::Error> {
        let gid = block.stream;
        let most = crate::tcp::TO_HOST.saturating_sub(self.held());
        let decoded = block.decode(&mut self.guest_hop.decoder, most)?;
        if let Some(s) = self.streams.get_mut(&gid) {
            if !s.guest.receiving() {
                return Err(stream_error(
                    gid,
                    Code::StreamClosed,
                    "a field block after its END_STREAM",
                ));
            }
            if !block.end_stream {
                return Err(stream_error(
                    gid,
                    Code::Protocol,
                    "a trailer section without END_STREAM",
                ));
            }
            let hpack::Decoded::Fields(fields) = decoded else {
                return Err(stream_error(
                    gid,
                    Code::EnhanceYourCalm,
                    "a trailer section past what the connection holds",
                ));
            };
            s.guest = s.guest.received_end();
            if !matches!(s.up, Upward::Answered) {
                s.up_last = Some(Last::Trailers(fields));
            }
            return Ok(());
        }
        if gid.is_multiple_of(2) || gid <= self.last_guest {
            return Err(http2::connection(
                Code::Protocol,
                "a stream the guest cannot open (§5.1.1)",
            ));
        }
        self.last_guest = gid;
        if self.guest_phase == Phase::Away {
            http2::rst_stream(gid, Code::RefusedStream as u32, &mut self.to_guest);
            return Ok(());
        }
        self.streams.insert(gid, Stream::new(block.end_stream));
        let fields = match decoded {
            hpack::Decoded::TooLarge => {
                self.answer(gid, Refusal::TooLarge);
                return Ok(());
            }
            hpack::Decoded::Fields(fields) => fields,
        };
        let go = match http2::request(fields, &run.credentials.grants, &self.sni) {
            http2::Request::Malformed(why) => return Err(stream_error(gid, Code::Protocol, why)),
            http2::Request::Misdirected { host } => {
                let sni = self.sni.clone();
                self.answer(gid, Refusal::Misdirected { sni, host });
                return Ok(());
            }
            http2::Request::Go(go) => go,
        };
        let path = go
            .fields
            .iter()
            .find(|f| f.name == b":path")
            .map(|f| String::from_utf8_lossy(&f.value).into_owned())
            .unwrap_or_default();
        if go.fills.iter().any(|f| mints(run, &f.reference, &go.host, &path)) {
            self.answer(gid, Refusal::Mints { host: go.host });
            return Ok(());
        }
        if let Some(s) = self.streams.get_mut(&gid) {
            s.up = Upward::Parked(Box::new(go));
        }
        Ok(())
    }

    /// The guest's stream `gid` answered here with `refusal`'s status and text, its request
    /// never sent on (or no more), what it held of the guest's let go.
    fn answer(&mut self, gid: u32, refusal: Refusal) {
        let (status, _) = refusal.status();
        say(
            &self.sni,
            format_args!("stream {gid} answered {status}: {refusal}"),
        );
        let Some(s) = self.streams.get_mut(&gid) else {
            return;
        };
        if let Upward::Sent { id, .. } = s.up {
            self.by_host.remove(&id);
            self.host_hop.close(id);
        }
        let body = format!("shards: {refusal}\n").into_bytes();
        let fields = [
            hpack::Field::new(
                b":status",
                status.to_string().as_bytes(),
                hpack::Indexing::Incremental,
            ),
            hpack::Field::new(
                b"content-type",
                b"text/plain; charset=utf-8",
                hpack::Indexing::Incremental,
            ),
            hpack::Field::new(
                b"content-length",
                body.len().to_string().as_bytes(),
                hpack::Indexing::Incremental,
            ),
        ];
        let mut block = Vec::new();
        self.guest_hop.encoder.encode(&fields, &mut block);
        http2::headers(
            gid,
            &block,
            false,
            self.guest_hop.peer.max_frame_size,
            &mut self.to_guest,
        );
        self.guest_hop.released(gid, s.up_data.len(), &mut self.to_guest);
        s.up_data.clear();
        s.up_last = None;
        s.up = Upward::Answered;
        s.down = Down::Body;
        s.down_data = body;
        s.down_last = Some(Last::End);
    }

    /// A parked request goes once each value it carries is leased and the host has room for
    /// another stream: encoded on the host's hop on the next host stream, each fill audited;
    /// answered 503 where a lease is refused, 502 where the host's side failed, and refused
    /// (§8.7) where the host has gone away.
    fn go(&mut self, gid: u32, run: &mut Run<'_>) {
        let Some(Upward::Parked(go)) = self.streams.get(&gid).map(|s| &s.up) else {
            return;
        };
        let references: Vec<String> = go.references().into_iter().map(str::to_string).collect();
        if let Some(why) = self.failed.clone() {
            return self.answer(gid, Refusal::Upstream(why));
        }
        if self.host_phase == Phase::Away {
            return self.refuse_stream(gid);
        }
        let mut waiting = false;
        for reference in references {
            match run.credentials.leases.ask(&reference, run.waiter, run.now) {
                Asked::Held(_) => {}
                Asked::Waiting => waiting = true,
                Asked::Refused(why) => return self.answer(gid, Refusal::Unleased { reference, why }),
            }
        }
        let open = self
            .streams
            .values()
            .filter(|s| matches!(s.up, Upward::Sent { state, .. } if state != StreamState::Closed))
            .count();
        let room = self
            .host_hop
            .peer
            .max_concurrent_streams
            .is_none_or(|most| open < most as usize);
        if waiting || !room {
            return;
        }
        let id = self.next_host;
        if id > http2::MOST_STREAM {
            // The host's stream ids spent (§5.1.1): a new connection is the guest's to make.
            return self.refuse_stream(gid);
        }
        let Some(s) = self.streams.get_mut(&gid) else {
            return;
        };
        let Upward::Parked(go) = std::mem::replace(&mut s.up, Upward::Answered) else {
            return;
        };
        let leases = &run.credentials.leases;
        let Some(block) = go.encode(&mut self.host_hop.encoder, |r| leases.held(r, run.now)) else {
            let reference = go
                .references()
                .first()
                .map_or_else(String::new, |r| r.to_string());
            let why = lease::Refusal::Lapsed;
            return self.answer(gid, Refusal::Unleased { reference, why });
        };
        let end = !s.guest.receiving() && s.up_data.is_empty() && matches!(s.up_last, Some(Last::End));
        if end {
            s.up_last = None;
        }
        http2::headers(
            id,
            &block,
            end,
            self.host_hop.peer.max_frame_size,
            &mut self.to_host,
        );
        drop(block);
        self.next_host = id.saturating_add(2);
        let at = SystemTime::now();
        s.mask.filled.clear();
        for fill in &go.fills {
            if !s.mask.filled.iter().any(|(r, _)| r == &fill.reference) {
                run.audit.push(Audit {
                    host: go.host.clone(),
                    reference: fill.reference.clone(),
                    what: Did::Filled,
                    at,
                });
                s.mask.filled.push((fill.reference.clone(), 0));
            }
        }
        s.up = Upward::Sent {
            id,
            state: if end {
                StreamState::HalfClosedLocal
            } else {
                StreamState::Open
            },
        };
        self.by_host.insert(id, gid);
    }

    /// The guest's DATA held for a sent request, to the host as its windows let it, then its
    /// END_STREAM or trailers; each octet sent given back to the guest's window.
    fn flush_up(&mut self, gid: u32) {
        let Some(s) = self.streams.get_mut(&gid) else {
            return;
        };
        let Upward::Sent { id, state } = &mut s.up else {
            return;
        };
        if !state.sending() || (s.up_data.is_empty() && s.up_last.is_none()) {
            return;
        }
        let end = matches!(s.up_last, Some(Last::End));
        let n = self.host_hop.send_data(*id, &s.up_data, end, &mut self.to_host);
        s.up_data.drain(..n.min(s.up_data.len()));
        self.guest_hop.released(gid, n, &mut self.to_guest);
        if !s.up_data.is_empty() {
            return;
        }
        match s.up_last.take() {
            Some(Last::End) => *state = state.sent_end(),
            Some(Last::Trailers(fields)) => {
                let mut block = Zeroizing::new(Vec::new());
                self.host_hop.encoder.encode(&fields, &mut block);
                http2::headers(
                    *id,
                    &block,
                    true,
                    self.host_hop.peer.max_frame_size,
                    &mut self.to_host,
                );
                *state = state.sent_end();
            }
            None => {}
        }
    }

    /// A stream's response DATA held, to the guest as its windows let it, then its END_STREAM
    /// or trailers; each octet sent given back to the host's window. A response answered here
    /// before its request ended ends the stream (§8.1: RST_STREAM NO_ERROR).
    fn flush_down(&mut self, gid: u32) {
        let Some(s) = self.streams.get_mut(&gid) else {
            return;
        };
        if !s.guest.sending() || (s.down_data.is_empty() && s.down_last.is_none()) {
            return;
        }
        let end = matches!(s.down_last, Some(Last::End));
        let n = self
            .guest_hop
            .send_data(gid, &s.down_data, end, &mut self.to_guest);
        s.down_data.drain(..n.min(s.down_data.len()));
        if let Upward::Sent { id, .. } = s.up {
            self.host_hop.released(id, n, &mut self.to_host);
        }
        if !s.down_data.is_empty() {
            return;
        }
        match s.down_last.take() {
            Some(Last::End) => {}
            Some(Last::Trailers(fields)) => {
                let mut block = Vec::new();
                self.guest_hop.encoder.encode(&fields, &mut block);
                http2::headers(
                    gid,
                    &block,
                    true,
                    self.guest_hop.peer.max_frame_size,
                    &mut self.to_guest,
                );
            }
            None => return,
        }
        s.guest = s.guest.sent_end();
        if matches!(s.up, Upward::Answered) && s.guest.receiving() {
            http2::rst_stream(gid, Code::NoError as u32, &mut self.to_guest);
            s.guest = StreamState::Closed;
        }
    }

    fn host_frame(
        &mut self,
        head: http2::Head,
        payload: &[u8],
        run: &mut Run<'_>,
    ) -> Result<(), http2::Error> {
        if let Some(mut block) = self.host_block.take() {
            if block.continued(head, payload, crate::tcp::TO_GUEST)? {
                return self.response_block(block, run);
            }
            self.host_block = Some(block);
            return Ok(());
        }
        if self.host_phase == Phase::Settings {
            if head.kind != http2::SETTINGS || head.flags & http2::ACK != 0 {
                return Err(http2::connection(
                    Code::Protocol,
                    "the host's preface is no SETTINGS",
                ));
            }
            self.host_phase = Phase::Open;
        }
        match head.kind {
            http2::SETTINGS => self.host_hop.settings(head, payload, &mut self.to_host),
            http2::WINDOW_UPDATE => self.window_update(Which::Host, head, payload),
            http2::PING => ping(head, payload, &mut self.to_host),
            http2::HEADERS => {
                let (block, whole) = http2::Block::headers(head, payload)?;
                if whole {
                    return self.response_block(block, run);
                }
                self.host_block = Some(block);
                Ok(())
            }
            http2::CONTINUATION => Err(http2::connection(Code::Protocol, "a CONTINUATION after no block")),
            http2::DATA => self.host_data(head, payload, run),
            http2::RST_STREAM => self.host_rst(head, payload),
            http2::GOAWAY => self.host_goaway(head, payload),
            http2::PUSH_PROMISE => Err(http2::connection(
                Code::Protocol,
                "a PUSH_PROMISE with push off (§8.4)",
            )),
            // PRIORITY is deprecated (§5.3.2) and every other type unknown here (§5.5).
            _ => Ok(()),
        }
    }

    /// The host's stream `hid`'s guest stream, where it is still carried: none for one let go
    /// of; an idle stream's is a connection error (§5.1).
    fn carried(&self, hid: u32) -> Result<Option<u32>, http2::Error> {
        if hid.is_multiple_of(2) || hid >= self.next_host {
            return Err(http2::connection(
                Code::Protocol,
                "a frame on a stream the injector never opened",
            ));
        }
        Ok(self.by_host.get(&hid).copied())
    }

    /// A response's block (§8.1), or its trailers: decoded on the host's hop, its echoes masked,
    /// and encoded on the guest's, in order with every other block that hop is sent.
    fn response_block(&mut self, block: http2::Block, run: &mut Run<'_>) -> Result<(), http2::Error> {
        let hid = block.stream;
        let decoded = block.decode(&mut self.host_hop.decoder, crate::tcp::TO_GUEST)?;
        let Some(gid) = self.carried(hid)? else {
            return Ok(());
        };
        let Some(s) = self.streams.get_mut(&gid) else {
            return Ok(());
        };
        let Upward::Sent { state, .. } = &mut s.up else {
            return Ok(());
        };
        if !state.receiving() {
            return Err(stream_error(
                hid,
                Code::StreamClosed,
                "a field block after its END_STREAM",
            ));
        }
        let hpack::Decoded::Fields(mut fields) = decoded else {
            return Err(stream_error(
                hid,
                Code::Internal,
                "a response's fields past what the connection holds",
            ));
        };
        let trailers = s.down == Down::Body;
        if trailers && !block.end_stream {
            return Err(stream_error(
                hid,
                Code::Protocol,
                "a trailer section without END_STREAM",
            ));
        }
        if block.end_stream {
            *state = state.received_end();
        }
        if trailers {
            let Stream { mask, down_data, .. } = &mut *s;
            let _ = mask.end_message(&mut |b| {
                down_data.extend_from_slice(b);
                Ok(())
            });
        }
        for f in &mut fields {
            f.value = masked(&mut s.mask, &f.value, run);
        }
        if trailers {
            s.down_last = Some(Last::Trailers(fields));
            return Ok(());
        }
        let informational = fields
            .iter()
            .find(|f| f.name == b":status")
            .is_some_and(|f| f.value.first() == Some(&b'1'));
        if !informational {
            s.down = Down::Body;
        }
        if !s.guest.sending() {
            return Ok(());
        }
        let mut out = Vec::new();
        self.guest_hop.encoder.encode(&fields, &mut out);
        http2::headers(
            gid,
            &out,
            block.end_stream,
            self.guest_hop.peer.max_frame_size,
            &mut self.to_guest,
        );
        if block.end_stream {
            s.guest = s.guest.sent_end();
        }
        Ok(())
    }

    fn host_data(
        &mut self,
        head: http2::Head,
        payload: &[u8],
        run: &mut Run<'_>,
    ) -> Result<(), http2::Error> {
        if head.stream == 0 {
            return Err(http2::connection(Code::Protocol, "DATA on stream 0"));
        }
        self.host_hop.received(head)?;
        let data = http2::data(head, payload)?;
        let length = head.length as usize;
        let carried = self.carried(head.stream)?;
        let Some(s) = carried.and_then(|gid| self.streams.get_mut(&gid)) else {
            // A stream let go of here: its DATA dropped, its credit given back.
            self.host_hop.released(head.stream, length, &mut self.to_host);
            self.host_hop.close(head.stream);
            return Ok(());
        };
        let Upward::Sent { state, .. } = &mut s.up else {
            return Ok(());
        };
        if !state.receiving() {
            return Err(stream_error(
                head.stream,
                Code::StreamClosed,
                "DATA after its END_STREAM",
            ));
        }
        if s.down == Down::Head {
            return Err(stream_error(
                head.stream,
                Code::Protocol,
                "DATA before a response's head",
            ));
        }
        let end = head.flags & http2::END_STREAM != 0;
        if end {
            *state = state.received_end();
        }
        let mut dropped = length.saturating_sub(data.len());
        if s.guest.sending() {
            let Stream {
                mask,
                down_data,
                down_last,
                ..
            } = &mut *s;
            let mut out = |b: &[u8]| {
                down_data.extend_from_slice(b);
                Ok(())
            };
            let _ = mask.push(data, true, run, &mut out);
            if end {
                let _ = mask.end_message(&mut out);
                *down_last = Some(Last::End);
            }
        } else {
            dropped = length;
        }
        self.host_hop.released(head.stream, dropped, &mut self.to_host);
        Ok(())
    }

    fn host_rst(&mut self, head: http2::Head, payload: &[u8]) -> Result<(), http2::Error> {
        let (true, Some(code)) = (
            head.stream != 0,
            http2::word(payload, 0).filter(|_| payload.len() == 4),
        ) else {
            return Err(http2::connection(
                Code::FrameSize,
                "a RST_STREAM on stream 0 or not of four octets",
            ));
        };
        if let Some(gid) = self.carried(head.stream)?
            && let Some(s) = self.streams.remove(&gid)
        {
            self.forget(gid, &s);
            if s.guest != StreamState::Closed {
                http2::rst_stream(gid, code, &mut self.to_guest);
            }
        }
        self.host_hop.close(head.stream);
        Ok(())
    }

    /// The host's GOAWAY (§6.8): the streams it will not process, and those not yet sent, refused
    /// to the guest, which may retry them (§8.7); the guest told the connection goes, with the
    /// host's code.
    fn host_goaway(&mut self, head: http2::Head, payload: &[u8]) -> Result<(), http2::Error> {
        let (true, Some(last), Some(code)) = (
            head.stream == 0,
            http2::word(payload, 0).map(|l| l & http2::MOST_STREAM),
            http2::word(payload, 4),
        ) else {
            return Err(http2::connection(
                Code::FrameSize,
                "a GOAWAY on a stream or short",
            ));
        };
        self.host_phase = Phase::Away;
        let refused: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, s)| match s.up {
                Upward::Sent { id, .. } => id > last,
                Upward::Parked(_) => true,
                Upward::Answered => false,
            })
            .map(|(&gid, _)| gid)
            .collect();
        for gid in refused {
            self.refuse_stream(gid);
        }
        if self.guest_phase != Phase::Away {
            http2::goaway(self.last_guest, code, &mut self.to_guest);
            self.guest_phase = Phase::Away;
        }
        Ok(())
    }
}

/// The injector's cost, measured (#74 step 10 part B): ignored tests ROOT runs, its README beside it.
#[cfg(test)]
#[path = "../../../docs/research/measurements/s33-injector-ab/ab.rs"]
mod ab;

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
        leaf_config(root, &[GRANTED, OTHER, FREE])
    }

    /// A test server's TLS, any suite: a leaf for `names` that `root` signs.
    fn leaf_config(root: &CertifiedIssuer<'static, KeyPair>, names: &[&str]) -> Arc<ServerConfig> {
        let mut p = params(names);
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
            sign_ins: signin::SignIns::new(Vec::new()),
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
        guest_offering(trusts, name, suites, &[])
    }

    /// A guest's TLS client as `guest` makes it, offering the application protocols `alpn`.
    fn guest_offering(
        trusts: CertificateDer<'static>,
        name: &str,
        suites: Option<Vec<rustls::SupportedCipherSuite>>,
        alpn: &[&[u8]],
    ) -> ClientConnection {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(trusts).unwrap();
        let mut provider = aws_lc_rs::default_provider();
        if let Some(suites) = suites {
            provider.cipher_suites = suites;
        }
        let mut config = ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
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
        /// The token its run resumes it by.
        waiter: u64,
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
                waiter: 7,
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
                waiter: self.waiter,
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
                        assert_eq!(r.waiter, self.waiter);
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
    fn echo_mask() -> Vec<u8> {
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
            holds(&bench.got, &echo_mask()),
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
        let masked = swapped(&head, PLACEHOLDER.as_bytes(), &echo_mask());
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
            &echo_mask()[..10],
            b"\r\n1f\r\n",
            &echo_mask()[10..],
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

    // Step 8: a sign-in through the injector (D242), its token endpoint simulated.

    const SIGN_IN: &str = "claude/acct-1";
    const TOKEN_HOST: &str = "platform.claude.com";
    const API_HOST: &str = "api.anthropic.com";
    /// The provider's tokens, shaped as Anthropic's and none real: as its code's exchange issues
    /// them, and as a refresh rotates them.
    const ISSUED_ACCESS: &str = "sk-ant-oat01-issued-access-not-real-0123456789";
    const ISSUED_REFRESH: &str = "sk-ant-ort01-issued-refresh-not-real-0123456789";
    const ROTATED_ACCESS: &str = "sk-ant-oat01-rotated-access-not-real-9876543210";
    const ROTATED_REFRESH: &str = "sk-ant-ort01-rotated-refresh-not-real-9876543210";
    /// What the broker gives in their places.
    const SIGNED_ACCESS: &str =
        "kiln-placeholder-aaaa456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIGNED_REFRESH: &str =
        "kiln-placeholder-bbbb456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn access_reference() -> String {
        format!("sign-ins/{SIGN_IN}/access")
    }

    /// A token response as Claude Code reads one (`formatTokens`).
    fn token_response(access: &str, refresh: &str) -> String {
        format!(
            r#"{{"token_type":"Bearer","access_token":"{access}","expires_in":28800,"refresh_token":"{refresh}","scope":"user:inference user:profile"}}"#
        )
    }

    /// A token endpoint as Claude Code's is (`TOKEN_URL`), over TLS `config` gives: each request
    /// told the test, head and body; a code's exchange answered with the issued tokens, anything
    /// else 400.
    fn token_endpoint(config: Arc<ServerConfig>) -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { return };
                let (config, tx) = (config.clone(), tx.clone());
                std::thread::spawn(move || {
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
                        let mut body = vec![0u8; len];
                        if tls.read_exact(&mut body).is_err() {
                            return;
                        }
                        let exchange = holds(&body, br#""grant_type":"authorization_code""#);
                        let _ = tx.send([head, body].concat());
                        let (status, answer) = if exchange {
                            ("200 OK", token_response(ISSUED_ACCESS, ISSUED_REFRESH))
                        } else {
                            ("400 Bad Request", r#"{"error":"unexpected"}"#.to_string())
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n{answer}",
                            answer.len()
                        );
                        if tls.write_all(response.as_bytes()).is_err() {
                            return;
                        }
                        let _ = tls.flush();
                    }
                });
            }
        });
        (addr, rx)
    }

    /// A run with Claude Code's sign-in granted, its access token toward its API host alone, its
    /// injector's host side trusting `root`; captured before where `captured` says.
    fn sign_in_credentials(root: &CertifiedIssuer<'static, KeyPair>, captured: bool) -> Credentials {
        let captured = captured.then(|| signin::Captured {
            access: access_reference(),
            access_placeholder: SIGNED_ACCESS.into(),
            refresh_placeholder: SIGNED_REFRESH.into(),
        });
        let grants: Vec<Grant> = captured.iter().map(|c| c.grant(&[API_HOST.into()])).collect();
        Credentials {
            leases: lease::Leases::new(grants.iter().map(|g| g.reference.clone()).collect()),
            grants,
            ca: ca::Ca::new(ca::Key::generate().unwrap(), &[API_HOST, TOKEN_HOST]).unwrap(),
            upstream: Upstream::new(vec![root.der().clone()]).unwrap(),
            sign_ins: signin::SignIns::new(vec![signin::SignInGrant {
                sign_in: SIGN_IN.into(),
                cli: Cli::ClaudeCode,
                toward: vec![API_HOST.into()],
                captured,
            }]),
        }
    }

    /// The run host and its orchestrator's broker, as a test lessor: each capture's tokens kept
    /// and the broker's placeholders given; each refresh one call to the provider, which rotates
    /// the tokens and whose answer, real tokens and all, comes back as it came; each revocation
    /// counted, the stored tokens kept; each lease of the access token answered with the stored
    /// one.
    #[derive(Default)]
    struct Broker {
        access: Vec<u8>,
        refresh: Vec<u8>,
        provider_calls: usize,
        revoked: usize,
    }

    impl Broker {
        fn holding(access: &str, refresh: &str) -> Broker {
            Broker {
                access: access.as_bytes().to_vec(),
                refresh: refresh.as_bytes().to_vec(),
                ..Broker::default()
            }
        }

        /// Answers what the run asks; how many asks.
        fn serve(&mut self, credentials: &mut Credentials) -> usize {
            let mut n = 0;
            for ask in credentials.leases.asks() {
                let (id, reference) = lease::decode_ask(&ask).unwrap();
                assert_eq!(
                    reference,
                    access_reference(),
                    "a lease of what no capture granted"
                );
                let life = NonZeroU64::new(60_000_000_000).unwrap();
                let answer = lease::encode_answer(id, Ok((life, &self.access))).unwrap();
                assert!(credentials.leases.answered(&answer, Instant::now()));
                n += 1;
            }
            for told in credentials.sign_ins.asks() {
                n += 1;
                let answered = match signin::Told::decode(&told).unwrap() {
                    signin::Told::Capture { id, sign_in, taken } => {
                        assert_eq!(sign_in, SIGN_IN);
                        assert_eq!(taken.expires_in, Some(28800));
                        self.access = taken.access.to_vec();
                        self.refresh = taken.refresh.unwrap().to_vec();
                        signin::Answered::Captured {
                            id,
                            result: Ok(signin::Captured {
                                access: access_reference(),
                                access_placeholder: SIGNED_ACCESS.into(),
                                refresh_placeholder: SIGNED_REFRESH.into(),
                            }),
                        }
                    }
                    signin::Told::Refresh { id, sign_in } => {
                        assert_eq!(sign_in, SIGN_IN);
                        assert!(!self.refresh.is_empty(), "a refresh of a sign-in never kept");
                        self.provider_calls += 1;
                        self.access = ROTATED_ACCESS.as_bytes().to_vec();
                        self.refresh = ROTATED_REFRESH.as_bytes().to_vec();
                        signin::Answered::Refreshed {
                            id,
                            result: Ok(Zeroizing::new(
                                token_response(ROTATED_ACCESS, ROTATED_REFRESH).into_bytes(),
                            )),
                        }
                    }
                    signin::Told::Revoke { sign_in } => {
                        assert_eq!(sign_in, SIGN_IN);
                        self.revoked += 1;
                        continue;
                    }
                };
                let changed = credentials
                    .sign_ins
                    .answered(&answered.encode().unwrap(), &credentials.grants)
                    .unwrap();
                crate::follow(credentials, changed);
            }
            n
        }
    }

    /// Turns `bench` until `done` says so of what its guest read, the broker answering what its
    /// run asks and the flow looking for its answers each turn.
    fn drive(
        bench: &mut Bench,
        credentials: &mut Credentials,
        broker: &mut Broker,
        done: impl Fn(&Bench) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(bench) {
            assert!(
                Instant::now() < deadline,
                "no end: the guest read {:?}",
                String::from_utf8_lossy(&bench.got)
            );
            let moved = bench.turn(credentials);
            let asked = broker.serve(credentials);
            // Whichever flow an answer named, this one looks: another bench's answer may have
            // been taken by an earlier drive, and a look with none for it does nothing.
            let _ = (credentials.leases.resumed(), credentials.sign_ins.resumed());
            bench.step(credentials, None, |f, run, q| f.resume(run, q));
            if !moved && asked == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn post(target: &str, host: &str, body: &str) -> Vec<u8> {
        format!(
            "POST {target} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept-Encoding: gzip, br\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn refresh_body() -> String {
        format!(
            r#"{{"grant_type":"refresh_token","refresh_token":"{SIGNED_REFRESH}","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","scope":"user:inference user:profile"}}"#
        )
    }

    /// Do: sign Claude Code in through the injector: its code exchanged at its token host, which
    /// answers real-shaped tokens; a request to its API host with the access placeholder the
    /// guest was given; on the first connection, a refresh and then a revocation with its refresh
    /// placeholder. Expect: the exchange reaching the host asking `identity`; its response held
    /// until the broker keeps its tokens, then reaching the guest with the broker's placeholders
    /// in their places, framed by its new length, no real token in it; the API request filled with
    /// the kept token; the refresh never reaching the host, one call to the provider outside, the
    /// guest given the same placeholders, kept from caches, and no rotated token; the held lease
    /// of the access token let go, its value rotated; the revocation answered 200 here, the kept
    /// tokens as they were; each audited by the sign-in's name, no token or placeholder in it.
    #[test]
    fn a_sign_in_is_taken_outside_and_its_refresh_and_revocation_answered_there() {
        let root = root("shards test root");
        let (token_addr, at_token) = token_endpoint(leaf_config(&root, &[TOKEN_HOST]));
        let (api_addr, at_api) = echo(leaf_config(&root, &[API_HOST]), false);
        let mut credentials = sign_in_credentials(&root, false);
        let ca = credentials.ca.der().to_vec();
        let mut broker = Broker::default();
        let mut cli = Bench::new(guest(ca.clone().into(), TOKEN_HOST, None), token_addr);
        let exchange = post(
            "/v1/oauth/token",
            TOKEN_HOST,
            r#"{"grant_type":"authorization_code","code":"c0de","redirect_uri":"https://platform.claude.com/oauth/code/callback","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","code_verifier":"v","state":"s"}"#,
        );
        cli.guest.writer().write_all(&exchange).unwrap();
        drive(&mut cli, &mut credentials, &mut broker, |b| whole(&b.got));
        let sent = at_token.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(holds(&sent, b"Accept-Encoding: identity\r\n") && !holds(&sent, b"gzip"));
        assert_eq!(
            broker.access,
            ISSUED_ACCESS.as_bytes(),
            "the issued token was not kept"
        );
        assert_eq!(broker.refresh, ISSUED_REFRESH.as_bytes());
        let answer = String::from_utf8(cli.got.clone()).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(
            answer.ends_with(&token_response(SIGNED_ACCESS, SIGNED_REFRESH)),
            "{answer}"
        );
        for real in [ISSUED_ACCESS, ISSUED_REFRESH] {
            assert!(
                !holds(&cli.got, real.as_bytes()),
                "a real token reached the guest"
            );
        }
        // Its access token is granted toward its API host now, filled in where its placeholder
        // stands.
        let mut api = Bench::new(guest(ca.clone().into(), API_HOST, None), api_addr);
        let request = format!(
            "GET /v1/models HTTP/1.1\r\nHost: {API_HOST}\r\nAuthorization: Bearer {SIGNED_ACCESS}\r\n\r\n"
        );
        api.guest.writer().write_all(request.as_bytes()).unwrap();
        drive(&mut api, &mut credentials, &mut broker, |b| whole(&b.got));
        let filled = at_api.recv_timeout(Duration::from_secs(10)).unwrap().request;
        assert_eq!(
            filled,
            swapped(
                request.as_bytes(),
                SIGNED_ACCESS.as_bytes(),
                ISSUED_ACCESS.as_bytes()
            )
        );
        assert!(
            !holds(&api.got, ISSUED_ACCESS.as_bytes()),
            "the token's echo reached the guest"
        );
        assert!(
            credentials
                .leases
                .held(&access_reference(), Instant::now())
                .is_some()
        );
        // Its refresh, answered outside.
        cli.got.clear();
        cli.guest
            .writer()
            .write_all(&post("/v1/oauth/token", TOKEN_HOST, &refresh_body()))
            .unwrap();
        drive(&mut cli, &mut credentials, &mut broker, |b| whole(&b.got));
        assert_eq!(broker.provider_calls, 1);
        let answer = String::from_utf8(cli.got.clone()).unwrap();
        assert!(answer.contains("Cache-Control: no-store\r\n"), "{answer}");
        assert!(
            answer.ends_with(&token_response(SIGNED_ACCESS, SIGNED_REFRESH)),
            "{answer}"
        );
        for real in [ISSUED_ACCESS, ISSUED_REFRESH, ROTATED_ACCESS, ROTATED_REFRESH] {
            assert!(
                !holds(&cli.got, real.as_bytes()),
                "a real token reached the guest"
            );
        }
        assert!(
            credentials
                .leases
                .held(&access_reference(), Instant::now())
                .is_none(),
            "the rotated token's old value is held still"
        );
        // Its revocation, answered here.
        cli.got.clear();
        let revoke = format!(
            r#"{{"token":"{SIGNED_REFRESH}","token_type_hint":"refresh_token","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e"}}"#
        );
        cli.guest
            .writer()
            .write_all(&post("/v1/oauth/token/revoke", TOKEN_HOST, &revoke))
            .unwrap();
        drive(&mut cli, &mut credentials, &mut broker, |b| whole(&b.got));
        assert!(cli.got.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(broker.revoked, 1);
        assert_eq!(
            broker.refresh,
            ROTATED_REFRESH.as_bytes(),
            "the kept token was revoked"
        );
        assert!(
            at_token.try_recv().is_err(),
            "a refresh or a revocation reached the provider"
        );
        let did: Vec<Did> = cli.audit.iter().map(|a| a.what).collect();
        assert_eq!(did, [Did::Captured, Did::Refreshed, Did::Revoked]);
        for record in cli.audit.iter().chain(&api.audit) {
            let said = record.to_string();
            assert!(
                said.contains(SIGN_IN) || said.contains(&access_reference()),
                "{said}"
            );
            for secret in [
                ISSUED_ACCESS,
                ISSUED_REFRESH,
                ROTATED_ACCESS,
                SIGNED_ACCESS,
                SIGNED_REFRESH,
            ] {
                assert!(!said.contains(secret), "{said}");
            }
        }
    }

    /// Do: two connections of the run's Claude Code, captured before (a later run's), ask its
    /// refresh before either is answered. Expect: one ask of the run host and one call to the
    /// provider outside; both answered with the sign-in's placeholders, nothing reaching the host.
    #[test]
    fn refreshes_of_one_sign_in_at_once_ask_once() {
        let root = root("shards test root");
        let (token_addr, at_token) = token_endpoint(leaf_config(&root, &[TOKEN_HOST]));
        let mut credentials = sign_in_credentials(&root, true);
        let ca = credentials.ca.der().to_vec();
        let mut broker = Broker::holding(ISSUED_ACCESS, ISSUED_REFRESH);
        let mut one = Bench::new(guest(ca.clone().into(), TOKEN_HOST, None), token_addr);
        let mut two = Bench::new(guest(ca.into(), TOKEN_HOST, None), token_addr);
        two.waiter = 8;
        let waiting = |bench: &mut Bench, credentials: &mut Credentials, flows: usize| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while credentials.sign_ins.waiting() < flows {
                assert!(Instant::now() < deadline, "the refresh was not asked");
                if !bench.turn(credentials) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        };
        one.guest
            .writer()
            .write_all(&post("/v1/oauth/token", TOKEN_HOST, &refresh_body()))
            .unwrap();
        waiting(&mut one, &mut credentials, 1);
        two.guest
            .writer()
            .write_all(&post("/v1/oauth/token", TOKEN_HOST, &refresh_body()))
            .unwrap();
        waiting(&mut two, &mut credentials, 2);
        assert_eq!(broker.serve(&mut credentials), 1, "two asks of one refresh");
        assert_eq!(broker.provider_calls, 1);
        for bench in [&mut one, &mut two] {
            drive(bench, &mut credentials, &mut broker, |b| whole(&b.got));
            assert!(
                bench
                    .got
                    .ends_with(token_response(SIGNED_ACCESS, SIGNED_REFRESH).as_bytes())
            );
            assert!(
                !holds(&bench.got, ROTATED_ACCESS.as_bytes())
                    && !holds(&bench.got, ROTATED_REFRESH.as_bytes())
            );
        }
        assert_eq!(broker.provider_calls, 1);
        assert!(at_token.try_recv().is_err(), "a refresh reached the provider");
    }

    /// Do: ask the endpoint that mints a key with a captured sign-in's access placeholder.
    /// Expect: answered 403 by the injector, nothing reaching the host, nothing leased.
    #[test]
    fn a_sign_ins_token_never_goes_to_the_endpoint_that_mints_a_key() {
        let root = root("shards test root");
        let (api_addr, at_api) = echo(leaf_config(&root, &[API_HOST]), false);
        let mut credentials = sign_in_credentials(&root, true);
        let mut broker = Broker::holding(ISSUED_ACCESS, ISSUED_REFRESH);
        let mut api = Bench::new(
            guest(credentials.ca.der().to_vec().into(), API_HOST, None),
            api_addr,
        );
        let request = format!(
            "POST /api/oauth/claude_cli/%63reate_api_key HTTP/1.1\r\nHost: {API_HOST}\r\nAuthorization: Bearer {SIGNED_ACCESS}\r\nContent-Length: 0\r\n\r\n"
        );
        api.guest.writer().write_all(request.as_bytes()).unwrap();
        drive(&mut api, &mut credentials, &mut broker, |b| {
            b.closed && whole(&b.got)
        });
        assert!(api.got.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
        assert!(at_api.try_recv().is_err(), "the request reached the host");
        assert!(
            credentials.leases.asks().is_empty()
                && credentials
                    .leases
                    .held(&access_reference(), Instant::now())
                    .is_none()
        );
    }

    // HTTP/2 through the injector (#74 step 7 slice (b)): a guest and a host that speak it, each
    // an end written over http2.rs and hpack.rs, as no HTTP/2 crate is in the tree.

    use crate::hpack::{self, Indexing};
    use crate::http2::{self, End, Holds};
    use std::collections::BTreeMap;

    /// What an HTTP/2 end in a test read.
    #[derive(Debug)]
    enum Event {
        Headers {
            stream: u32,
            fields: Vec<hpack::Field>,
            end: bool,
        },
        Data {
            stream: u32,
            bytes: Vec<u8>,
            end: bool,
        },
        Rst {
            stream: u32,
            code: u32,
        },
        GoAway {
            last: u32,
            code: u32,
        },
    }

    /// One end of an HTTP/2 connection: its hop, what it has read and not framed, a block being
    /// read, the DATA it is to send as its peer's windows let it, and what it has to write. It
    /// gives credit back for DATA as it reads it, and holds its peer to its windows.
    struct Peer {
        hop: http2::Hop,
        inbox: Vec<u8>,
        preface: bool,
        block: Option<http2::Block>,
        sending: BTreeMap<u32, (Vec<u8>, bool)>,
        out: Vec<u8>,
        events: Vec<Event>,
    }

    impl Peer {
        fn new(end: End, holds: Holds) -> Peer {
            let hop = http2::Hop::new(holds, 4096);
            let mut out = Vec::new();
            hop.preface(end, &mut out);
            Peer {
                hop,
                inbox: Vec::new(),
                preface: end == End::Client,
                block: None,
                sending: BTreeMap::new(),
                out,
                events: Vec::new(),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.inbox.extend_from_slice(bytes);
            if !self.preface {
                if self.inbox.len() < http2::PREFACE.len() {
                    return;
                }
                assert!(self.inbox.starts_with(http2::PREFACE), "no client preface");
                self.inbox.drain(..http2::PREFACE.len());
                self.preface = true;
            }
            let most = http2::Settings::INITIAL.max_frame_size;
            while let Some((head, payload)) = http2::frame(&self.inbox, most).unwrap() {
                let payload = payload.to_vec();
                self.inbox.drain(..http2::HEAD + payload.len());
                self.frame(head, &payload);
            }
            self.pump();
        }

        fn frame(&mut self, head: http2::Head, payload: &[u8]) {
            if let Some(mut block) = self.block.take() {
                if block.continued(head, payload, usize::MAX).unwrap() {
                    self.decoded(block);
                } else {
                    self.block = Some(block);
                }
                return;
            }
            match head.kind {
                http2::SETTINGS => self.hop.settings(head, payload, &mut self.out).unwrap(),
                http2::WINDOW_UPDATE => self.hop.window_update(head, payload).unwrap(),
                http2::PING if head.flags & http2::ACK == 0 => http2::ping_ack(payload, &mut self.out),
                http2::HEADERS => {
                    let (block, whole) = http2::Block::headers(head, payload).unwrap();
                    if whole {
                        self.decoded(block);
                    } else {
                        self.block = Some(block);
                    }
                }
                http2::DATA => {
                    self.hop.received(head).unwrap();
                    let bytes = http2::data(head, payload).unwrap().to_vec();
                    self.hop
                        .released(head.stream, head.length as usize, &mut self.out);
                    self.events.push(Event::Data {
                        stream: head.stream,
                        bytes,
                        end: head.flags & http2::END_STREAM != 0,
                    });
                }
                http2::RST_STREAM => self.events.push(Event::Rst {
                    stream: head.stream,
                    code: http2::word(payload, 0).unwrap(),
                }),
                http2::GOAWAY => self.events.push(Event::GoAway {
                    last: http2::word(payload, 0).unwrap(),
                    code: http2::word(payload, 4).unwrap(),
                }),
                http2::PUSH_PROMISE => panic!("a push, with push off"),
                _ => {}
            }
        }

        fn decoded(&mut self, block: http2::Block) {
            let fields = match block.decode(&mut self.hop.decoder, usize::MAX).unwrap() {
                hpack::Decoded::Fields(f) => f,
                other => panic!("{other:?}"),
            };
            self.events.push(Event::Headers {
                stream: block.stream,
                fields,
                end: block.end_stream,
            });
        }

        fn send_headers(&mut self, stream: u32, fields: &[hpack::Field], end: bool) {
            let mut block = Vec::new();
            self.hop.encoder.encode(fields, &mut block);
            http2::headers(stream, &block, end, self.hop.peer.max_frame_size, &mut self.out);
        }

        fn send_data(&mut self, stream: u32, bytes: &[u8], end: bool) {
            let entry = self.sending.entry(stream).or_default();
            entry.0.extend_from_slice(bytes);
            entry.1 |= end;
            self.pump();
        }

        fn pump(&mut self) {
            let streams: Vec<u32> = self.sending.keys().copied().collect();
            for stream in streams {
                let (bytes, end) = self.sending.remove(&stream).unwrap();
                let n = self.hop.send_data(stream, &bytes, end, &mut self.out);
                if n < bytes.len() {
                    self.sending.insert(stream, (bytes[n..].to_vec(), end));
                }
            }
        }

        fn take_out(&mut self) -> Vec<u8> {
            std::mem::take(&mut self.out)
        }

        /// Each RST_STREAM (its stream and code) and GOAWAY (its last stream and code) it read.
        fn ends(&self) -> Vec<(&'static str, u32, u32)> {
            self.events
                .iter()
                .filter_map(|e| match *e {
                    Event::Rst { stream, code } => Some(("RST_STREAM", stream, code)),
                    Event::GoAway { last, code } => Some(("GOAWAY", last, code)),
                    _ => None,
                })
                .collect()
        }

        /// A stream's response: its head's fields, its data, and whether it ended.
        fn response(&self, stream: u32) -> Got {
            let (mut fields, mut data, mut ended) = (Vec::new(), Vec::new(), false);
            for e in &self.events {
                match e {
                    Event::Headers {
                        stream: s,
                        fields: f,
                        end,
                    } if *s == stream => {
                        if fields.is_empty() {
                            fields = f.iter().map(|f| (f.name.clone(), f.value.to_vec())).collect();
                        }
                        ended |= end;
                    }
                    Event::Data {
                        stream: s,
                        bytes,
                        end,
                    } if *s == stream => {
                        data.extend_from_slice(bytes);
                        ended |= end;
                    }
                    _ => {}
                }
            }
            Got { fields, data, ended }
        }
    }

    /// What an HTTP/2 end in a test read of a stream's response.
    struct Got {
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        data: Vec<u8>,
        ended: bool,
    }

    fn field(name: &str, value: &[u8]) -> hpack::Field {
        hpack::Field::new(name.as_bytes(), value, Indexing::Incremental)
    }

    fn value_of<'f>(fields: &'f [(Vec<u8>, Vec<u8>)], name: &str) -> Option<&'f [u8]> {
        fields
            .iter()
            .find(|(n, _)| n == name.as_bytes())
            .map(|(_, v)| &v[..])
    }

    /// What the HTTP/2 test host had of a request: its fields as its decoder read them, how each
    /// was sent too, and its body.
    struct H2Had {
        fields: Vec<hpack::Field>,
        body: Vec<u8>,
    }

    /// The test host's windows: small, so that a large body meets them on each stream and on the
    /// connection.
    const HOST_HOLDS: Holds = Holds {
        stream: 4096,
        connection: 65_535,
    };

    /// An HTTPS server speaking HTTP/2 alone (ALPN `h2`), its windows `HOST_HOLDS`, answering
    /// each request once it has ended: `:status` 200, an `x-echo` of its Authorization, and DATA
    /// of its Authorization, a newline and its body; telling the test what it had.
    fn h2_host(root: &CertifiedIssuer<'static, KeyPair>) -> (SocketAddr, mpsc::Receiver<H2Had>) {
        let mut config = (*leaf_config(root, &[GRANTED, OTHER])).clone();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { return };
                let (config, tx) = (config.clone(), tx.clone());
                std::thread::spawn(move || h2_answer(config, tcp, tx));
            }
        });
        (addr, rx)
    }

    fn h2_answer(config: Arc<ServerConfig>, tcp: TcpStream, tx: mpsc::Sender<H2Had>) {
        let mut tls = rustls::StreamOwned::new(ServerConnection::new(config).unwrap(), tcp);
        let mut peer = Peer::new(End::Server, HOST_HOLDS);
        let mut requests: BTreeMap<u32, (Vec<hpack::Field>, Vec<u8>)> = BTreeMap::new();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let out = peer.take_out();
            if !out.is_empty() && (tls.write_all(&out).is_err() || tls.flush().is_err()) {
                return;
            }
            let n = match tls.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            peer.feed(&buf[..n]);
            for event in std::mem::take(&mut peer.events) {
                let (stream, end) = match event {
                    Event::Headers { stream, fields, end } => {
                        requests.entry(stream).or_default().0 = fields;
                        (stream, end)
                    }
                    Event::Data { stream, bytes, end } => {
                        requests.entry(stream).or_default().1.extend_from_slice(&bytes);
                        (stream, end)
                    }
                    _ => continue,
                };
                if !end {
                    continue;
                }
                let (fields, body) = requests.remove(&stream).unwrap();
                let auth = fields
                    .iter()
                    .find(|f| f.name == b"authorization")
                    .map(|f| f.value.to_vec())
                    .unwrap_or_default();
                peer.send_headers(stream, &[field(":status", b"200"), field("x-echo", &auth)], false);
                peer.send_data(stream, &[&auth[..], b"\n", &body].concat(), true);
                let _ = tx.send(H2Had { fields, body });
            }
        }
    }

    /// The guest's windows: small too, so that a large response meets them.
    const GUEST_HOLDS: Holds = Holds {
        stream: 8192,
        connection: 65_535,
    };

    /// A request's fields as a guest sends them, its Authorization indexed, as RFC 7541 §7.1.3
    /// lets a client.
    fn h2_request(
        method: &str,
        authority: &str,
        path: &str,
        authorization: Option<&str>,
    ) -> Vec<hpack::Field> {
        let mut fields = vec![
            field(":method", method.as_bytes()),
            field(":scheme", b"https"),
            field(":authority", authority.as_bytes()),
            field(":path", path.as_bytes()),
        ];
        if let Some(a) = authorization {
            fields.push(field("authorization", a.as_bytes()));
        }
        fields
    }

    /// Turns the bench with an HTTP/2 guest until `done`, every byte the guest read kept in
    /// `seen`, leases answered as they are asked.
    fn h2_until(
        bench: &mut Bench,
        peer: &mut Peer,
        seen: &mut Vec<u8>,
        credentials: &mut Credentials,
        done: impl Fn(&Peer) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(peer) {
            assert!(
                Instant::now() < deadline,
                "no end: the guest read {:?}",
                peer.events
            );
            let out = peer.take_out();
            if !out.is_empty() {
                bench.guest.writer().write_all(&out).unwrap();
            }
            let moved = bench.turn(credentials);
            if !bench.got.is_empty() {
                let got = std::mem::take(&mut bench.got);
                seen.extend_from_slice(&got);
                peer.feed(&got);
            }
            if lease_all(credentials) > 0 {
                for r in credentials.leases.resumed() {
                    assert_eq!(r.waiter, bench.waiter);
                }
                bench.step(credentials, None, |f, run, q| f.resume(run, q));
            } else if !moved {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn h2_bench(credentials: &Credentials, addr: SocketAddr) -> (Bench, Peer) {
        let guest = guest_offering(
            credentials.ca.der().to_vec().into(),
            GRANTED,
            None,
            &[b"h2", b"http/1.1"],
        );
        (Bench::new(guest, addr), Peer::new(End::Client, GUEST_HOLDS))
    }

    /// Do: a guest offering `h2` and `http/1.1` sends a host speaking HTTP/2 alone, on one
    /// connection, stream 1 with the credential's placeholder as a bearer token (indexed, as the
    /// guest's encoder chose) and stream 3 at once with a body of 1 MiB and no credential, both
    /// hops' windows small; then stream 5 with the placeholder again. Expect: HTTP/2 on both hops;
    /// stream 1 arriving with the value in its Authorization, sent never indexed (RFC 7541
    /// §7.1.3), its echo in a field and in DATA back to the guest masked with the placeholder's
    /// bytes, the value never reaching the guest; stream 3's body arriving and coming back as it
    /// was sent; stream 5 never indexed again; one lease asked and each fill audited.
    #[test]
    fn an_h2_guests_granted_stream_arrives_filled_never_indexed_and_its_echo_comes_back_masked() {
        let root = root("shards test root");
        let (addr, had) = h2_host(&root);
        let mut credentials = credentials(&root);
        let (mut bench, mut peer) = h2_bench(&credentials, addr);
        let mut seen = Vec::new();
        let bearer = format!("Bearer {PLACEHOLDER}");
        let body: Vec<u8> = (0..1u32 << 20).map(|i| (i % 251) as u8).collect();
        peer.send_headers(
            1,
            &h2_request("POST", GRANTED, "/v1/messages", Some(&bearer)),
            false,
        );
        peer.send_data(1, b"hello", true);
        peer.send_headers(3, &h2_request("PUT", GRANTED, "/upload", None), false);
        peer.send_data(3, &body, true);
        h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, |p| {
            p.response(1).ended && p.response(3).ended
        });
        assert_eq!(
            bench.guest.alpn_protocol(),
            Some(&b"h2"[..]),
            "the guest was not served HTTP/2"
        );
        let mut arrived: Vec<H2Had> = (0..2)
            .map(|_| had.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        arrived.sort_by_key(|h| h.body.len());
        let auth = arrived[0]
            .fields
            .iter()
            .find(|f| f.name == b"authorization")
            .unwrap();
        assert_eq!(&auth.value[..], &[b"Bearer ", VALUE].concat()[..]);
        assert_eq!(
            auth.indexing,
            Indexing::Never,
            "the credential was not sent never indexed"
        );
        assert_eq!(arrived[0].body, b"hello");
        assert!(arrived[1].body == body, "the large body changed on its way");
        let masked = [b"Bearer ", &echo_mask()[..]].concat();
        let Got { fields, data, .. } = peer.response(1);
        assert_eq!(value_of(&fields, ":status"), Some(&b"200"[..]));
        assert_eq!(
            value_of(&fields, "x-echo"),
            Some(&masked[..]),
            "the echo in a field was not masked"
        );
        assert_eq!(
            data,
            [&masked[..], b"\nhello"].concat(),
            "the echo in DATA was not masked"
        );
        let Got { fields, data, .. } = peer.response(3);
        let sent = [b"\n", &body[..]].concat();
        let differs = data.iter().zip(&sent).position(|(a, b)| a != b);
        assert!(
            data == sent,
            "the large body came back changed: {} octets of {}, first differing at {differs:?}: {:?} {:?}",
            data.len(),
            sent.len(),
            value_of(&fields, ":status").map(String::from_utf8_lossy),
            String::from_utf8_lossy(&data[..data.len().min(120)])
        );
        assert!(!holds(&seen, VALUE), "the value reached the guest");
        // The same connection's next stream: its value in no table of either hop.
        peer.send_headers(5, &h2_request("GET", GRANTED, "/v1/models", Some(&bearer)), true);
        h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, |p| {
            p.response(5).ended
        });
        let five = had.recv_timeout(Duration::from_secs(10)).unwrap();
        let auth = five.fields.iter().find(|f| f.name == b"authorization").unwrap();
        assert_eq!(
            (&auth.value[..], auth.indexing),
            (&[b"Bearer ", VALUE].concat()[..], Indexing::Never)
        );
        assert!(!holds(&seen, VALUE), "the value reached the guest");
        assert!(
            credentials.leases.asks().is_empty(),
            "a held lease was asked again"
        );
        assert_eq!(peer.ends(), [], "a stream reset or the connection ended");
        assert_eq!(bench.audit.len(), 2, "{:?}", bench.audit);
        assert!(
            bench
                .audit
                .iter()
                .all(|a| (a.host.as_str(), a.reference.as_str(), a.what) == (GRANTED, CLAUDE, Did::Filled))
        );
    }

    /// Do: on one HTTP/2 connection to the granted host, stream 1 for another host (granted
    /// another credential) carrying the placeholder, and stream 3 for the granted host. Expect:
    /// stream 1 answered 421 by the injector on that stream alone, nothing of it reaching the host
    /// and nothing leased; stream 3 answered by the host on the same connection (RFC 9110 §7.4).
    #[test]
    fn an_h2_stream_for_another_host_is_answered_421_while_another_completes() {
        let root = root("shards test root");
        let (addr, had) = h2_host(&root);
        let mut credentials = credentials(&root);
        let (mut bench, mut peer) = h2_bench(&credentials, addr);
        let mut seen = Vec::new();
        let bearer = format!("Bearer {PLACEHOLDER}");
        peer.send_headers(1, &h2_request("GET", OTHER, "/", Some(&bearer)), true);
        peer.send_headers(3, &h2_request("GET", GRANTED, "/v1/models", None), true);
        h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, |p| {
            p.response(1).ended && p.response(3).ended
        });
        assert_eq!(bench.guest.alpn_protocol(), Some(&b"h2"[..]));
        let Got { fields, data, .. } = peer.response(1);
        assert_eq!(value_of(&fields, ":status"), Some(&b"421"[..]));
        assert!(
            String::from_utf8_lossy(&data).contains("RFC 9110 §7.4"),
            "{data:?}"
        );
        let Got { fields, .. } = peer.response(3);
        assert_eq!(value_of(&fields, ":status"), Some(&b"200"[..]));
        let one = had.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            one.fields
                .iter()
                .any(|f| f.name == b":path" && &f.value[..] == b"/v1/models")
        );
        assert!(
            had.recv_timeout(Duration::from_millis(200)).is_err(),
            "a misdirected stream reached the host"
        );
        assert!(credentials.leases.asks().is_empty() && bench.audit.is_empty());
        assert!(!bench.closed, "the connection ended with its 421");
        assert_eq!(peer.ends(), [], "a stream reset or the connection ended");
    }

    /// Do: a guest offering `h2` and `http/1.1` sends a granted host that speaks HTTP/1.1 alone
    /// (no ALPN) a request with the placeholder. Expect: the guest served HTTP/1.1, its request
    /// arriving filled as any HTTP/1.1 request's.
    #[test]
    fn a_guest_offering_h2_to_an_http_1_1_host_is_served_http_1_1() {
        let root = root("shards test root");
        let (addr, had) = echo(echo_config(&root), false);
        let mut credentials = credentials(&root);
        let (mut bench, _) = h2_bench(&credentials, addr);
        let request =
            format!("GET / HTTP/1.1\r\nHost: {GRANTED}\r\nx-api-key: {PLACEHOLDER}\r\n\r\n").into_bytes();
        bench.guest.writer().write_all(&request).unwrap();
        bench.until(&mut credentials, true, |b| whole(&b.got));
        assert_eq!(bench.guest.alpn_protocol(), Some(&b"http/1.1"[..]));
        assert_eq!(
            had.recv_timeout(Duration::from_secs(10)).unwrap().request,
            swapped(&request, PLACEHOLDER.as_bytes(), VALUE)
        );
    }
}
