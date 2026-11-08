//! TLS 1.3 between shards homes over QUIC (D180): each side presents its identity as a raw
//! public key (RFC 7250), an Ed25519 key signing the handshake (RFC 8446 §4.4.3), on the
//! application protocol [`ALPN`] (RFC 9001 §8.1). A client pins the daemon's key by its
//! SHA-256; a daemon takes any client's key at the handshake and judges it after, by its
//! enrollment or by the invitation it joins with ([`crate::invite`]). Early data is off on
//! both sides: a first request may change what the daemon holds.

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
use hyper_quic::{ClientConfig, IdleTimeout, ServerConfig, TransportConfig, VarInt};

use crate::Error;
use crate::identity::{self, Fingerprint, Identity};

/// The application protocol a shards connection speaks (RFC 7301).
pub const ALPN: &[u8] = b"shards/1";

/// Longest a connection waits for its handshake, and a stranger for its join (server.rs),
/// with a keep-alive every quarter of it, as focal's transport keeps its connections
/// (../focal crates/focal-wire transport.rs): a live peer is heard from several times in each
/// window however late its scheduler runs it, and a bound as short as a request's deadline
/// lost every connection of a process stalled for half a second on a loaded host there. A
/// restarted daemon is told sooner, by its stateless reset (RFC 9000 §10.3). A connection
/// that has done its handshake is held to [`QUIET`] instead (net.rs, `Liveness`).
pub const IDLE: Duration = Duration::from_secs(10);

/// Longest an established connection goes unheard before it is taken for dead: its
/// transport's idle timeout (RFC 9000 §10.1, `max_idle_timeout`), which every packet its peer
/// sends restarts, each acknowledgment of the bytes it carries among them, so that a
/// connection lives while its peer is heard and ends this long after it last was. TCP's own
/// floor for giving up on data it sent and has heard nothing of, RFC 1122 §4.2.3.5's R2, "at
/// least 100 seconds" (Linux waits 924.6 s by default, ip-sysctl `tcp_retries2`): the TCP
/// beside a move on a 2.4 GHz Wi-Fi link carried its bytes on where this bound at [`IDLE`]
/// lost 2 moves of 20 mid-send, one "timed out" here and one "reset by peer" (its daemon
/// had let it go first, RFC 9000 §10.3), the link working throughout (D176 part 6). Each
/// move logs the longest silence its connection lived through (`Seen`), the evidence this
/// bound is held to. The peer's bound counts too, as a connection takes the lesser of the
/// two (RFC 9000 §10.1): a daemon of an earlier build ends one at [`IDLE`].
pub const QUIET: Duration = Duration::from_secs(100);

fn provider() -> &'static rustls::crypto::CryptoProvider {
    hyper_quic::crypto::rustls::node_provider()
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

/// The transport both sides run: idle after [`QUIET`] unheard, its handshake held to [`IDLE`]
/// by its driver (net.rs), kept alive four times within [`IDLE`], as RFC 9000 §10.1.2 has an
/// endpoint that awaits its peer's answer send PINGs to defer its peer's idle timeout (and
/// more often than the 30 s it finds most middleboxes keep a UDP flow's state for), and only
/// the streams the protocol has, bidirectional ones a client opens, a command or a join each.
/// A peer may open no stream the other would not read (RFC 9000 §4.6), whose bytes its side
/// would hold: none one way, and none of a client's. A daemon's clients may have quinn-proto's
/// 100 open at once.
fn transport(serves: bool) -> Result<TransportConfig, Error> {
    let mut transport = TransportConfig::default();
    let idle = IdleTimeout::try_from(QUIET).map_err(|_| Error::new("an idle bound QUIC cannot carry"))?;
    transport.max_idle_timeout(Some(idle));
    transport.keep_alive_interval(Some(IDLE / 4));
    transport.max_concurrent_uni_streams(VarInt::from_u32(0));
    if !serves {
        transport.max_concurrent_bidi_streams(VarInt::from_u32(0));
    }
    Ok(transport)
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

/// The server side, which a daemon listening for clients accepts connections under.
pub fn server(identity: &Identity) -> Result<ServerConfig, Error> {
    let resolver = AlwaysResolvesServerRawPublicKeys::new(certified(identity)?);
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::new(format!("TLS: {e}")))?
        .with_client_cert_verifier(Box::new(AnyClient))
        .with_cert_resolver(Box::new(resolver));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let crypto = QuicServerConfig::try_from(tls).map_err(|e| Error::new(format!("QUIC's TLS: {e}")))?;
    let mut config = ServerConfig::with_crypto(Box::new(crypto));
    config.transport_config(transport(true)?);
    Ok(config)
}

/// The client side, which connects to the daemon whose key's SHA-256 is `daemon`.
pub fn client(identity: &Identity, daemon: Fingerprint) -> Result<ClientConfig, Error> {
    let resolver = AlwaysResolvesClientRawPublicKeys::new(certified(identity)?);
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::new(format!("TLS: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Box::new(Pinned(daemon)))
        .with_client_cert_resolver(Box::new(resolver));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.enable_early_data = false;
    let crypto = QuicClientConfig::try_from(tls).map_err(|e| Error::new(format!("QUIC's TLS: {e}")))?;
    let mut config = ClientConfig::new(Box::new(crypto));
    config.transport_config(transport(false)?);
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
struct AnyClient;

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
        if identity::is_ed25519(end_entity) {
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
