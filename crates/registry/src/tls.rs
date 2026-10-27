//! TLS for registries (docs/research/registry-pull.md R2): rustls with AWS-LC, verified
//! as Docker's Go code verifies. On macOS and Windows the OS verifies, as Go delegates to
//! it there; on Linux, webpki checks against the system's bundle. Extra roots are the
//! per-registry CAs Docker reads from `certs.d`.

use std::sync::Arc;

#[cfg(target_os = "macos")]
use crate::apple::Verifier;
pub use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
#[cfg(not(target_os = "macos"))]
use rustls_platform_verifier::Verifier;

use crate::Error;
use crate::certs::ClientCertificate;

/// A client configuration that trusts the platform's roots and `extra_roots`, and
/// presents `client`'s certificate when a server asks for one.
/// - TLS 1.2 stays on: a Docker-operated CDN host still refuses TLS 1.3 (§6.2).
/// - Key exchange prefers X25519MLKEM768, which Docker Hub's token host and CDN offer.
/// - A Linux host with no CA certificates (a minimal image's, a guest's of its own) and no
///   root of the registry's own, for which the platform's verifier is not built
///   (rustls-platform-verifier, "No CA certificates were loaded from the system"), gets a
///   configuration all the same, which refuses every certificate saying so: as Go's, whose
///   pool is then empty and every verification fails (crypto/x509, SystemCertPool). So an
///   insecure registry over plain HTTP is still reached through the HTTPS try Docker makes
///   of one first, which its plain answer ends (`http.rs`); before, the pull failed at once.
pub fn client_config(
    extra_roots: Vec<CertificateDer<'static>>,
    client: Option<ClientCertificate>,
) -> Result<Arc<ClientConfig>, Error> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let none_given = extra_roots.is_empty();
    let verifier: Arc<dyn ServerCertVerifier> =
        match Verifier::new_with_extra_roots(extra_roots, provider.clone()) {
            Ok(verifier) => Arc::new(verifier),
            Err(rustls::Error::General(why)) if none_given && cfg!(target_os = "linux") => {
                Arc::new(Untrusting {
                    why,
                    schemes: provider.signature_verification_algorithms.supported_schemes(),
                })
            }
            Err(e) => return Err(e.into()),
        };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match client {
        Some((chain, key)) => builder.with_client_auth_cert(chain, key)?,
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// The verifier of a host that trusts no root ([`client_config`]): every certificate
/// refused, saying `why`.
#[derive(Debug)]
struct Untrusting {
    why: String,
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for Untrusting {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Err(rustls::Error::General(self.why.clone()))
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(self.why.clone()))
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(self.why.clone()))
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use rustls::pki_types::ServerName;
    use rustls::{
        ClientConnection, NamedGroup, ProtocolVersion, ServerConfig, ServerConnection, StreamOwned,
    };

    use crate::testing::registry;

    /// Serves one connection: reads 5 bytes and answers 5.
    fn serve_once(server: Arc<ServerConfig>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = StreamOwned::new(ServerConnection::new(server).unwrap(), tcp);
            let mut hello = [0u8; 5];
            if tls.read_exact(&mut hello).is_ok() {
                let _ = tls.write_all(b"world");
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        addr
    }

    /// Says hello over TLS and returns the connection and the server's reply.
    fn talk(
        config: Arc<ClientConfig>,
        addr: std::net::SocketAddr,
    ) -> Result<(StreamOwned<ClientConnection, TcpStream>, [u8; 5]), std::io::Error> {
        let name = ServerName::try_from("localhost").map_err(std::io::Error::other)?;
        let conn = ClientConnection::new(config, name).map_err(std::io::Error::other)?;
        let mut tls = StreamOwned::new(conn, TcpStream::connect(addr)?);
        tls.write_all(b"hello")?;
        let mut reply = [0u8; 5];
        tls.read_exact(&mut reply)?;
        Ok((tls, reply))
    }

    #[test]
    fn registries_are_trusted_through_their_certs_d_root_only() {
        let (ca, server) = registry(&[&rustls::version::TLS13]);
        // A host with no system roots at all builds one that refuses every certificate
        // (`Untrusting`), as one with roots refuses this CA.
        if let Ok(config) = client_config(Vec::new(), None) {
            let refused = talk(config, serve_once(server.clone()));
            assert!(refused.is_err(), "an unknown CA must be refused");
        }
        let (tls, reply) = talk(client_config(vec![ca], None).unwrap(), serve_once(server)).unwrap();
        assert_eq!(&reply, b"world");
        assert_eq!(tls.conn.protocol_version(), Some(ProtocolVersion::TLSv1_3));
        assert_eq!(
            tls.conn.negotiated_key_exchange_group().map(|g| g.name()),
            Some(NamedGroup::X25519MLKEM768)
        );
    }

    /// The verifier of a host that trusts no root refuses a registry's certificate at its
    /// handshake, saying why.
    #[test]
    fn a_host_that_trusts_no_root_refuses_every_certificate_saying_why() {
        let (_, server) = registry(&[&rustls::version::TLS13]);
        let provider = Arc::new(aws_lc_rs::default_provider());
        let untrusting = Untrusting {
            why: "no roots on this host".into(),
            schemes: provider.signature_verification_algorithms.supported_schemes(),
        };
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(untrusting))
            .with_no_client_auth();
        let refused = talk(Arc::new(config), serve_once(server)).err().unwrap();
        assert!(refused.to_string().contains("no roots on this host"), "{refused}");
    }

    #[test]
    fn tls_1_2_only_hosts_are_still_reached() {
        let (ca, server) = registry(&[&rustls::version::TLS12]);
        let (tls, reply) = talk(client_config(vec![ca], None).unwrap(), serve_once(server)).unwrap();
        assert_eq!(&reply, b"world");
        assert_eq!(tls.conn.protocol_version(), Some(ProtocolVersion::TLSv1_2));
    }
}
