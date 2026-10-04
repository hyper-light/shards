//! TLS for registries (docs/research/registry-pull.md R2): rustls with AWS-LC, verified
//! as Docker's Go code verifies. On macOS and Windows the OS verifies, as Go delegates to
//! it there; on Linux, webpki checks against the system's bundle. Extra roots are the
//! per-registry CAs Docker reads from `certs.d`.

use std::sync::Arc;

#[cfg(target_os = "macos")]
use crate::apple::Verifier;
pub use rustls::ClientConfig;
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::CertificateDer;
#[cfg(not(target_os = "macos"))]
use rustls_platform_verifier::Verifier;

use crate::Error;
use crate::certs::ClientCertificate;

/// A client configuration that trusts the platform's roots and `extra_roots`, and
/// presents `client`'s certificate when a server asks for one.
/// - TLS 1.2 stays on: a Docker-operated CDN host still refuses TLS 1.3 (§6.2).
/// - Key exchange prefers X25519MLKEM768, which Docker Hub's token host and CDN offer.
pub fn client_config(
    extra_roots: Vec<CertificateDer<'static>>,
    client: Option<ClientCertificate>,
) -> Result<Arc<ClientConfig>, Error> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let verifier = Verifier::new_with_extra_roots(extra_roots, provider.clone())?;
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier));
    let config = match client {
        Some((chain, key)) => builder.with_client_auth_cert(chain, key)?,
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
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
        // A host with no system roots at all cannot even build the configuration.
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

    #[test]
    fn tls_1_2_only_hosts_are_still_reached() {
        let (ca, server) = registry(&[&rustls::version::TLS12]);
        let (tls, reply) = talk(client_config(vec![ca], None).unwrap(), serve_once(server)).unwrap();
        assert_eq!(&reply, b"world");
        assert_eq!(tls.conn.protocol_version(), Some(ProtocolVersion::TLSv1_2));
    }
}
