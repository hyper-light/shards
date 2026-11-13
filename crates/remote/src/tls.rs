//! TLS 1.3 between shards homes over QUIC (D180): each side presents its identity as a raw
//! public key (RFC 7250), an Ed25519 key signing the handshake (RFC 8446 §4.4.3), on the
//! application protocol [`ALPN`] (RFC 9001 §8.1). A client pins the daemon's key by its
//! SHA-256; a daemon takes any client's key at the handshake and judges it after, by its
//! enrollment or by the invitation it joins with ([`crate::invite`]). Early data is off on
//! both sides: a first request may change what the daemon holds. Every link, both ways, is
//! TLS13_AES_256_GCM_SHA384 ([`provider`]).

use std::sync::LazyLock;
use std::time::Duration;

use hyper_quic::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use hyper_quic::rustls::client::AlwaysResolvesClientRawPublicKeys;
use hyper_quic::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use hyper_quic::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use hyper_quic::rustls::server::AlwaysResolvesServerRawPublicKeys;
use hyper_quic::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use hyper_quic::rustls::sign::CertifiedKey;
use hyper_quic::rustls::{self, CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use hyper_quic::{ClientConfig, ServerConfig, TransportConfig, VarInt};

use crate::Error;
use crate::identity::{self, Fingerprint, Identity};

/// The application protocol a shards connection speaks (RFC 7301).
pub const ALPN: &[u8] = b"shards/1";

/// What this side knows of a peer from its connections before (R04), as its caller keeps it (a
/// file per peer in the home): nothing, for a peer never reached, or what its path has shown.
/// No figure of shards' own stands in for what a path has not shown (the person's ruling,
/// 2026-11-13): a cold start, and every wait after a run-out, follows the peer's liveness and
/// progress, and after that a wait's bound is the slowest answer seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// Never reached: its handshake starts from RFC 9002 §6.2.2's 333 ms ("When no previous RTT
    /// is available, the initial RTT SHOULD be set to 333 milliseconds") and has no deadline.
    Never,
    /// Reached, its last waits answered: its next handshake starts from its path's RTT and is
    /// given up past the slowest handshake seen from it.
    Reached(Path),
    /// A wait of its last connection ran out: its next handshake starts from its path's RTT and
    /// has no deadline, as a peer never reached's, until a handshake ends again.
    RanOut(Path),
}

/// What a peer's path has shown over its connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Path {
    /// The smoothed RTT its last connection ended with (RFC 9002 §5.3), which the next starts
    /// from (§6.2.2: "Resumed connections over the same network MAY use the previous
    /// connection's final smoothed RTT value as the resumed connection's initial RTT").
    pub rtt: Duration,
    /// The slowest handshake seen from it, from the first packet to its handshake's end.
    pub handshake: Duration,
    /// The slowest answer seen on it: the longest a send of this side's waited for the peer to
    /// acknowledge any of it.
    pub answer: Duration,
    /// The shortest silence after which a connection on it was found dead, its next send
    /// unanswered (a middlebox's state lost, or the peer gone): none until one was.
    pub lost: Option<Duration>,
}

impl Known {
    const NEVER_RTT: Duration = Duration::from_millis(333);

    /// What its path has shown, where it was reached.
    pub fn path(self) -> Option<Path> {
        match self {
            Known::Never => None,
            Known::Reached(path) | Known::RanOut(path) => Some(path),
        }
    }

    /// The RTT QUIC's loss recovery starts the handshake from (its `initial_rtt`).
    pub fn initial_rtt(self) -> Duration {
        self.path().map_or(Self::NEVER_RTT, |path| path.rtt)
    }

    /// How long its handshake is waited for: the slowest seen from it, where its last waits
    /// were answered; else for as long as its caller waits, as its probing goes on.
    pub fn handshake_deadline(self) -> Option<Duration> {
        match self {
            Known::Reached(path) => Some(path.handshake),
            Known::Never | Known::RanOut(_) => None,
        }
    }

    /// What is known after a connection that saw `seen`: a handshake that ended and the slowest
    /// answer it waited for raise the path's; the silence after which it was found dead, the
    /// shortest such; its RTT, the last; a run-out, marked. A connection whose handshake never
    /// ended leaves what was known, but for a run-out past the slowest handshake.
    pub fn after(self, seen: &crate::net::Seen) -> Known {
        let Some(handshake) = seen.handshake else {
            return match (self, seen.ran_out) {
                (Known::Reached(path), true) => Known::RanOut(path),
                (known, _) => known,
            };
        };
        let before = self.path();
        let lost = match (before.and_then(|p| p.lost), seen.lost) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let path = Path {
            rtt: seen.rtt,
            handshake: before.map_or(handshake, |p| p.handshake.max(handshake)),
            answer: before.map_or(seen.answer, |p| p.answer.max(seen.answer)),
            lost,
        };
        if seen.ran_out {
            Known::RanOut(path)
        } else {
            Known::Reached(path)
        }
    }
}

/// Longest an established connection goes unheard before it is taken for dead: its
/// transport's idle timeout (RFC 9000 §10.1, `max_idle_timeout`), which every packet its peer
/// sends restarts, each acknowledgment of the bytes it carries among them, so that a
/// connection lives while its peer is heard and ends this long after it last was. TCP's own
/// floor for giving up on data it sent and has heard nothing of, RFC 1122 §4.2.3.5's R2, "at
/// least 100 seconds" (Linux waits 924.6 s by default, ip-sysctl `tcp_retries2`): the TCP
/// beside a move on a 2.4 GHz Wi-Fi link carried its bytes on where this bound at an earlier
/// build's 10 s lost 2 moves of 20 mid-send, one "timed out" here and one "reset by peer" (its
/// daemon had let it go first, RFC 9000 §10.3), the link working throughout (D176 part 6). Each
/// move logs the longest silence its connection lived through (`Seen`), the evidence this
/// bound is held to. The peer's bound counts too, as a connection takes the lesser of the
/// two (RFC 9000 §10.1): a daemon of an earlier build ends one at 10 s, held by its own
/// keep-alive.
pub const QUIET: Duration = Duration::from_secs(100);

/// The provider of every shards QUIC link, a context's and the managed output's: hyper-quic's
/// node provider (aws-lc-rs, the hybrid post-quantum groups alone) narrowed to
/// TLS13_AES_256_GCM_SHA384, as Ada directed on 2026-11-11 ("all such communications should be
/// AESGCM256 bidirectional encrypted"; D180). TLS 1.3 negotiates one suite for both directions,
/// each keyed apart (RFC 8446 §4.1.3, §7.3), so a peer offering no AES-256-GCM is refused at
/// its handshake. Initial packets keep AES-128-GCM under keys any observer derives (RFC 9001
/// §5), which hyper-quic takes from aws-lc-rs whatever is offered here.
static PROVIDER: LazyLock<rustls::crypto::CryptoProvider> =
    LazyLock::new(|| rustls::crypto::CryptoProvider {
        cipher_suites: vec![rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384],
        ..hyper_quic::crypto::rustls::node_provider().clone()
    });

fn provider() -> &'static rustls::crypto::CryptoProvider {
    &PROVIDER
}

/// What one end's TLS 1.3 handshake negotiated, by IANA's codes: its cipher suite (RFC 8446
/// §4.1.3; "TLS Cipher Suites", 0x1302 for TLS13_AES_256_GCM_SHA384, the one [`provider`]
/// offers) and its key exchange group (§4.2.7; "TLS Supported Groups"), as that end's own TLS
/// state says them once its handshake is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub suite: u16,
    pub group: u16,
}

impl Negotiated {
    /// Whether its suite is one [`provider`] offers: TLS13_AES_256_GCM_SHA384, every shards
    /// link's (D180, D239).
    pub fn is_provided(&self) -> bool {
        provider()
            .cipher_suites
            .iter()
            .any(|offered| u16::from(offered.suite()) == self.suite)
    }

    /// The suite's name as rustls says it, none for a code it does not know.
    pub fn suite_name(&self) -> Option<&'static str> {
        rustls::CipherSuite::from(self.suite).as_str()
    }

    /// The group's name as rustls says it, none for a code it does not know.
    pub fn group_name(&self) -> Option<&'static str> {
        rustls::NamedGroup::from(self.group).as_str()
    }
}

fn certified(identity: &Identity) -> Result<CertifiedKey, Error> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.pkcs8().to_vec()));
    let signing = provider()
        .key_provider
        .load_private_key(key)
        .map_err(|e| Error::new(format!("this home's key: {e}")))?;
    Ok(CertifiedKey::new(
        vec![CertificateDer::from(identity.spki().to_vec())],
        signing,
    ))
}

/// How long a connection held to `bound` of silence, its idle timeout (RFC 9000 §10.1), goes
/// unheard before it sends a keep-alive, a PING for which its peer restarts its own idle timer
/// (§10.1.2): a quarter of `bound`. §10.1 makes an idle timeout last at least three probe
/// timeouts, for "multiple probes to be sent and lost, prior to idle timeout"; held to that
/// rule, `bound` takes three keep-alives, a quarter, a half and three quarters of it after the
/// last packet either way (quinn-proto restarts the interval at each packet sent or received),
/// and no more, as unneeded PINGs cost (§10.1.2). A live peer is lost only if all three are, or
/// a stall of three quarters of `bound` holds either side up; a dead one is let go at most a
/// quarter of `bound` past it, as only the first packet sent since the last one heard restarts
/// this side's own timer (§10.1). focal's transport keeps its connections so (../focal
/// crates/focal-wire transport.rs), and D180's were from the start.
pub(crate) fn keep_alive(bound: Duration) -> Duration {
    bound / 4
}

/// The transport both sides of a context's link run: no idle timeout of its own (RFC 9000
/// §10.1, §18.2: 0 is none) and no keep-alive of QUIC's, as its driver checks the peer's
/// liveness at use, by the slowest answer the path has shown, and keeps it alive only where the
/// path or the peer's own idle timer has shown it must (net.rs, `Liveness`); its handshake
/// starts from what is known of the peer ([`Known::initial_rtt`]). Only the streams the
/// protocol has, bidirectional ones a client opens, a command or a join each. A peer may open
/// no stream the other would not read (RFC 9000 §4.6), whose bytes its side would hold: none one
/// way, and none of a client's. A daemon's clients may have quinn-proto's 100 open at once.
fn transport(serves: bool, known: Known) -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(None);
    transport.keep_alive_interval(None);
    transport.initial_rtt(known.initial_rtt());
    transport.max_concurrent_uni_streams(VarInt::from_u32(0));
    if !serves {
        transport.max_concurrent_bidi_streams(VarInt::from_u32(0));
    }
    transport
}

/// A raw public key's signature over a TLS 1.3 handshake, by the provider's algorithms.
fn verify_signature(
    message: &[u8],
    key: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature_with_raw_key(
        message,
        &SubjectPublicKeyInfoDer::from(key.as_ref()),
        dss,
        &provider().signature_verification_algorithms,
    )
}

/// The server side, which a daemon listening for clients accepts connections under: a client's
/// round trip is not known before its handshake, which starts from a peer never measured's.
pub fn server(identity: &Identity) -> Result<ServerConfig, Error> {
    server_under(provider(), identity, None, transport(true, Known::Never), ALPN)
}

pub(crate) fn owner_server(
    identity: &Identity,
    owner: Fingerprint,
    transport: TransportConfig,
    alpn: &[u8],
) -> Result<ServerConfig, Error> {
    server_under(provider(), identity, Some(owner), transport, alpn)
}

fn server_under(
    provider: &'static rustls::crypto::CryptoProvider,
    identity: &Identity,
    owner: Option<Fingerprint>,
    transport: TransportConfig,
    alpn: &[u8],
) -> Result<ServerConfig, Error> {
    let resolver = AlwaysResolvesServerRawPublicKeys::new(certified(identity)?);
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::new(format!("TLS: {e}")))?
        .with_client_cert_verifier(Box::new(AnyClient(owner)))
        .with_cert_resolver(Box::new(resolver));
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.max_early_data_size = 0;
    let crypto = QuicServerConfig::try_from(tls).map_err(|e| Error::new(format!("QUIC's TLS: {e}")))?;
    let mut config = ServerConfig::with_crypto(Box::new(crypto));
    config.transport_config(transport);
    Ok(config)
}

/// The client side, which connects to the daemon whose key's SHA-256 is `daemon`, its
/// handshake starting from what is known of it.
pub fn client(identity: &Identity, daemon: Fingerprint, known: Known) -> Result<ClientConfig, Error> {
    client_under(provider(), identity, daemon, transport(false, known), ALPN)
}

pub(crate) fn client_with(
    identity: &Identity,
    daemon: Fingerprint,
    transport: TransportConfig,
    alpn: &[u8],
) -> Result<ClientConfig, Error> {
    client_under(provider(), identity, daemon, transport, alpn)
}

fn client_under(
    provider: &'static rustls::crypto::CryptoProvider,
    identity: &Identity,
    daemon: Fingerprint,
    transport: TransportConfig,
    alpn: &[u8],
) -> Result<ClientConfig, Error> {
    let resolver = AlwaysResolvesClientRawPublicKeys::new(certified(identity)?);
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::new(format!("TLS: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Box::new(Pinned(daemon)))
        .with_client_cert_resolver(Box::new(resolver));
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.enable_early_data = false;
    let crypto = QuicClientConfig::try_from(tls).map_err(|e| Error::new(format!("QUIC's TLS: {e}")))?;
    let mut config = ClientConfig::new(Box::new(crypto));
    config.transport_config(transport);
    Ok(config)
}

/// The name a client gives the daemon it connects to: none, as a pinned key names it.
pub const SERVER_NAME: &str = "shards";

/// A daemon known by its key's SHA-256 alone.
#[derive(Debug)]
struct Pinned(Fingerprint);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if identity::is_ed25519(end_entity) && identity::fingerprint(end_entity) == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// Any client's Ed25519 key, which the daemon judges once the handshake is done.
#[derive(Debug)]
struct AnyClient(Option<Fingerprint>);

impl ClientCertVerifier for AnyClient {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if identity::is_ed25519(end_entity)
            && self
                .0
                .is_none_or(|owner| identity::fingerprint(end_entity) == owner)
        {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use hyper_quic::rustls::crypto::CryptoProvider;
    use hyper_quic::rustls::crypto::aws_lc_rs::cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    };
    use hyper_quic::rustls::{AlertDescription, CipherSuite, PeerIncompatible, SupportedCipherSuite};
    use hyper_quic::{ConnectionError, TransportErrorCode};

    use super::*;
    use crate::handshake::{Side, connect};

    /// The node provider offering `suites`, in that order: a peer that shares everything with
    /// shards' links but its ciphers.
    fn offering(suites: &[SupportedCipherSuite]) -> CryptoProvider {
        CryptoProvider {
            cipher_suites: suites.to_vec(),
            ..hyper_quic::crypto::rustls::node_provider().clone()
        }
    }

    static CHACHA20: LazyLock<CryptoProvider> = LazyLock::new(|| offering(&[TLS13_CHACHA20_POLY1305_SHA256]));
    static AES_128: LazyLock<CryptoProvider> = LazyLock::new(|| offering(&[TLS13_AES_128_GCM_SHA256]));
    /// Go's crypto/tls order on hardware without AES-GCM (go1.26.1 src/crypto/tls/defaults.go,
    /// `defaultCipherSuitesTLS13NoAES`).
    static CHACHA20_FIRST: LazyLock<CryptoProvider> = LazyLock::new(|| {
        offering(&[
            TLS13_CHACHA20_POLY1305_SHA256,
            TLS13_AES_128_GCM_SHA256,
            TLS13_AES_256_GCM_SHA384,
        ])
    });

    /// The daemon's side and the client's of two handshakes: a client under `other` with a
    /// shards daemon, and a shards client with a daemon under `other`.
    fn either_way(other: &'static CryptoProvider) -> [(Side, Side); 2] {
        let daemon = Identity::generate().unwrap();
        let home = Identity::generate().unwrap();
        let theirs = client_under(
            other,
            &home,
            daemon.fingerprint(),
            transport(false, Known::Never),
            ALPN,
        );
        let to_ours = connect(server(&daemon).unwrap(), theirs.unwrap());
        let theirs = server_under(other, &daemon, None, transport(true, Known::Never), ALPN);
        let from_ours = connect(
            theirs.unwrap(),
            client(&home, daemon.fingerprint(), Known::Never).unwrap(),
        );
        [to_ours, from_ours]
    }

    /// Both ends connected, each reporting TLS13_AES_256_GCM_SHA384.
    fn assert_aes_256_gcm((daemon, client): &(Side, Side)) {
        assert!(
            daemon.connected && client.connected,
            "{:?} {:?}",
            daemon.lost,
            client.lost
        );
        assert_eq!(daemon.suite(), Some(CipherSuite::TLS13_AES_256_GCM_SHA384));
        assert_eq!(client.suite(), Some(CipherSuite::TLS13_AES_256_GCM_SHA384));
    }

    /// The daemon refused the handshake for want of a suite in common, TLS's handshake_failure
    /// (RFC 8446 §4.1.1), and told its client so as QUIC carries the alert (RFC 9001 §4.8).
    fn assert_refused_for_its_cipher((daemon, client): &(Side, Side)) {
        let handshake_failure = TransportErrorCode::crypto(AlertDescription::HandshakeFailure.into());
        assert!(
            !daemon.connected && !client.connected,
            "connected under {:?} and {:?}",
            daemon.suite(),
            client.suite()
        );
        match &daemon.lost {
            Some(ConnectionError::TransportError(refused)) => {
                assert_eq!(refused.code, handshake_failure);
                let cause = rustls::Error::PeerIncompatible(PeerIncompatible::NoCipherSuitesInCommon);
                assert_eq!(refused.reason, cause.to_string());
            }
            other => panic!("the daemon's side: {other:?}"),
        }
        match &client.lost {
            Some(ConnectionError::ConnectionClosed(close)) => assert_eq!(close.error_code, handshake_failure),
            other => panic!("the client's side: {other:?}"),
        }
    }

    /// Every link shards makes, a context's and the managed output's, is
    /// TLS13_AES_256_GCM_SHA384, as both its ends report.
    #[test]
    fn every_link_is_aes_256_gcm_at_both_ends() {
        let daemon = Identity::generate().unwrap();
        let home = Identity::generate().unwrap();
        let context = connect(
            server(&daemon).unwrap(),
            client(&home, daemon.fingerprint(), Known::Never).unwrap(),
        );
        let alpn = crate::managed::ALPN;
        let owner = owner_server(&daemon, home.fingerprint(), transport(true, Known::Never), alpn);
        let subscriber = client_with(&home, daemon.fingerprint(), transport(false, Known::Never), alpn);
        let managed = connect(owner.unwrap(), subscriber.unwrap());
        assert_aes_256_gcm(&context);
        assert_aes_256_gcm(&managed);
    }

    /// What a home knows of a peer (the person's ruling, 2026-11-13): a peer never reached
    /// starts its handshake from RFC 9002's 333 ms with no deadline; one reached, from its path's
    /// last RTT, given up past the slowest handshake seen from it; after a run-out, with no
    /// deadline again. A connection raises the slowest handshake and answer, keeps the shortest
    /// silence found dead and takes its last RTT; one whose handshake never ended leaves what
    /// was known, but marks a run-out past the slowest handshake.
    #[test]
    fn what_is_known_of_a_peer_follows_what_its_path_showed() {
        let ms = Duration::from_millis;
        assert_eq!(Known::Never.initial_rtt(), ms(333));
        assert_eq!(Known::Never.handshake_deadline(), None);
        let seen = |handshake, answer, lost, ran_out| crate::net::Seen {
            rtt: ms(20),
            handshake,
            answer,
            lost,
            ran_out,
            ..crate::net::Seen::default()
        };
        let first = Known::Never.after(&seen(Some(ms(40)), ms(30), None, false));
        let path = Path {
            rtt: ms(20),
            handshake: ms(40),
            answer: ms(30),
            lost: None,
        };
        assert_eq!(first, Known::Reached(path));
        assert_eq!(first.initial_rtt(), ms(20));
        assert_eq!(first.handshake_deadline(), Some(ms(40)));
        // A faster handshake and answer leave the slowest; a silence found dead is kept.
        let second = first.after(&seen(Some(ms(10)), ms(5), Some(ms(90_000)), true));
        let lost = Path {
            lost: Some(ms(90_000)),
            ..path
        };
        assert_eq!(second, Known::RanOut(lost));
        assert_eq!(second.handshake_deadline(), None);
        // The next that ends raises them, keeps the shorter silence, and is reached again.
        let third = second.after(&seen(Some(ms(70)), ms(60), Some(ms(120_000)), false));
        let raised = Path {
            handshake: ms(70),
            answer: ms(60),
            ..lost
        };
        assert_eq!(third, Known::Reached(raised));
        // A handshake never ended: nothing learned, but a run-out past the slowest is marked.
        assert_eq!(third.after(&seen(None, Duration::ZERO, None, false)), third);
        assert_eq!(
            third.after(&seen(None, Duration::ZERO, None, true)),
            Known::RanOut(raised)
        );
        assert_eq!(
            Known::Never.after(&seen(None, Duration::ZERO, None, true)),
            Known::Never
        );
    }

    /// A negotiation is the provider's only for TLS13_AES_256_GCM_SHA384, by IANA's code, and
    /// names its suite and group as rustls does; a code rustls does not know is named none.
    #[test]
    fn a_negotiation_is_the_providers_for_aes_256_gcm_alone() {
        let named = |suite: SupportedCipherSuite| Negotiated {
            suite: u16::from(suite.suite()),
            group: u16::from(rustls::NamedGroup::X25519MLKEM768),
        };
        let aes256 = named(TLS13_AES_256_GCM_SHA384);
        assert_eq!(aes256.suite, 0x1302);
        assert!(aes256.is_provided());
        assert_eq!(aes256.suite_name(), Some("TLS13_AES_256_GCM_SHA384"));
        assert_eq!(aes256.group_name(), Some("X25519MLKEM768"));
        for other in [TLS13_AES_128_GCM_SHA256, TLS13_CHACHA20_POLY1305_SHA256] {
            assert!(!named(other).is_provided(), "{:?}", other.suite());
        }
        // Codes rustls names none of, as it says first.
        let unknown = Negotiated {
            suite: 0x1399,
            group: 0xfefe,
        };
        assert!(matches!(
            CipherSuite::from(unknown.suite),
            CipherSuite::Unknown(_)
        ));
        assert!(matches!(
            rustls::NamedGroup::from(unknown.group),
            rustls::NamedGroup::Unknown(_)
        ));
        assert!(!unknown.is_provided());
        assert_eq!((unknown.suite_name(), unknown.group_name()), (None, None));
    }

    /// A peer that prefers ChaCha20-Poly1305 and AES-128-GCM and offers AES-256-GCM last is
    /// answered with AES-256-GCM, client or daemon: a shards daemon, which takes its client's
    /// order (hyper-tls `choose_suite_and_kx_group`), chose ChaCha20-Poly1305 for it before.
    #[test]
    fn a_peer_preferring_other_suites_gets_aes_256_gcm_either_way() {
        for link in either_way(&CHACHA20_FIRST) {
            assert_aes_256_gcm(&link);
        }
    }

    /// A peer limited to ChaCha20-Poly1305, which the node provider allowed, is refused at
    /// its handshake, client or daemon.
    #[test]
    fn a_peer_limited_to_chacha20_poly1305_is_refused_either_way() {
        for link in either_way(&CHACHA20) {
            assert_refused_for_its_cipher(&link);
        }
    }

    /// A peer limited to AES-128-GCM, the one suite every TLS 1.3 peer must implement (RFC
    /// 8446 §9.1), is refused at its handshake, client or daemon.
    #[test]
    fn a_peer_limited_to_aes_128_gcm_is_refused_either_way() {
        for link in either_way(&AES_128) {
            assert_refused_for_its_cipher(&link);
        }
    }

    /// A shards of before, under the node provider (AES-256-GCM first, then
    /// ChaCha20-Poly1305), still connects either way on AES-256-GCM: a context between a
    /// machine updated and one not yet keeps working.
    #[test]
    fn an_earlier_shards_still_connects_on_aes_256_gcm() {
        for link in either_way(hyper_quic::crypto::rustls::node_provider()) {
            assert_aes_256_gcm(&link);
        }
    }
}
