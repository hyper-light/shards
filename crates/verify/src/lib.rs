//! A TLS server's certificate verified as Docker's Go code verifies it (D19; docs/research/
//! registry-pull.md R2): on macOS and Windows the OS verifies, as Go delegates to it there; on
//! Linux, webpki checks against the system's bundle. The registry's client and the credential
//! injector's upstream side (D242) verify alike through it (R12).

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "macos")]
use apple::Verifier;
#[cfg(not(target_os = "macos"))]
use rustls_platform_verifier::Verifier;

/// A verifier that trusts the platform's roots and `extra_roots`, with `provider`'s signature
/// algorithms.
///
/// A Linux host with no CA certificates (a minimal image's, a guest's of its own) and no extra
/// root, for which the platform's verifier is not built (rustls-platform-verifier, "No CA
/// certificates were loaded from the system"), gets one all the same, which refuses every
/// certificate saying so: as Go's, whose pool is then empty and every verification fails
/// (crypto/x509, SystemCertPool).
///
/// On Linux the system's roots are read as it is made, so a process that confines itself makes
/// it first; elsewhere the OS is asked as each certificate is verified.
pub fn verifier(
    extra_roots: Vec<CertificateDer<'static>>,
    provider: Arc<CryptoProvider>,
) -> Result<Arc<dyn ServerCertVerifier>, rustls::Error> {
    let none_given = extra_roots.is_empty();
    match Verifier::new_with_extra_roots(extra_roots, provider.clone()) {
        Ok(verifier) => Ok(Arc::new(verifier)),
        Err(rustls::Error::General(why)) if none_given && cfg!(target_os = "linux") => {
            Ok(Arc::new(Untrusting {
                why,
                schemes: provider.signature_verification_algorithms.supported_schemes(),
            }))
        }
        Err(e) => Err(e),
    }
}

/// The platform's roots, for a verifier made later than they are read ([`verifier_of`]): the
/// credential injector's (D242), whose network process confines itself as it starts and is given
/// its run's roots after. On Linux the system's certificates, read as this is made from the
/// directories openssl-probe knows (`candidate_cert_dirs`), where rustls-platform-verifier finds
/// them through rustls-native-certs, but never from `SSL_CERT_FILE` or `SSL_CERT_DIR`, which
/// those read first: no variable of the process's environment adds a root, nor replaces the
/// system's. A distribution whose bundle lies outside those directories alone (openssl-probe's
/// OpenELEC and Entware files) gets fewer roots, never more. Elsewhere nothing is read: the OS
/// is asked as each certificate is verified, and reads no variable.
#[derive(Debug, Clone, Default)]
pub struct Platform {
    #[cfg(target_os = "linux")]
    roots: Vec<CertificateDer<'static>>,
}

impl Platform {
    pub fn read() -> Platform {
        #[cfg(target_os = "linux")]
        {
            let mut roots = Vec::new();
            for dir in openssl_probe::candidate_cert_dirs() {
                roots.extend(rustls_native_certs::load_certs_from_paths(None, Some(dir)).certs);
            }
            Platform { roots }
        }
        #[cfg(not(target_os = "linux"))]
        Platform {}
    }
}

/// A verifier as [`verifier`] makes one, trusting `platform`'s roots and `extra_roots`, none of
/// them from the environment. On Linux, rustls's WebPKI verifier over them, as
/// rustls-platform-verifier's is there (its `verification/others.rs`). Elsewhere [`verifier`]'s.
/// On every host a root of `extra_roots` that is no certificate is refused, as webpki parses a
/// trust anchor (rustls's `RootCertStore::add`), where macOS's verifier would hold it unread.
pub fn verifier_of(
    platform: &Platform,
    extra_roots: Vec<CertificateDer<'static>>,
    provider: Arc<CryptoProvider>,
) -> Result<Arc<dyn ServerCertVerifier>, rustls::Error> {
    #[cfg(target_os = "linux")]
    {
        let mut store = rustls::RootCertStore::empty();
        for root in extra_roots {
            store.add(root)?;
        }
        store.add_parsable_certificates(platform.roots.iter().cloned());
        if store.is_empty() {
            return Ok(Arc::new(Untrusting {
                why: "No CA certificates were loaded from the system".into(),
                schemes: provider.signature_verification_algorithms.supported_schemes(),
            }));
        }
        let built = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(store), provider)
            .build()
            .map_err(|e| rustls::Error::Other(rustls::OtherError(Arc::new(e))))?;
        Ok(built)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Platform {} = platform;
        for root in &extra_roots {
            rustls::RootCertStore::empty().add(root.clone())?;
        }
        verifier(extra_roots, provider)
    }
}

/// The verifier of a host that trusts no root ([`verifier`]): every certificate refused,
/// saying `why`.
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
    use rustls::crypto::aws_lc_rs;

    /// The verifier of a host that trusts no root refuses every certificate and every
    /// handshake signature, saying why.
    #[test]
    fn a_host_that_trusts_no_root_refuses_every_certificate_saying_why() {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let untrusting = Untrusting {
            why: "no roots on this host".into(),
            schemes: provider.signature_verification_algorithms.supported_schemes(),
        };
        let cert = CertificateDer::from(vec![0x30, 0x03, 0x01, 0x02, 0x03]);
        let name = ServerName::try_from("localhost").unwrap();
        let refused = untrusting
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .err()
            .unwrap();
        assert!(refused.to_string().contains("no roots on this host"), "{refused}");
        assert!(!untrusting.supported_verify_schemes().is_empty());
    }

    /// A root given trusts the certificates it signs for their names, and the platform's
    /// verifier refuses one it did not sign, whatever the host trusts.
    #[test]
    fn an_extra_root_is_trusted_for_what_it_signs() {
        use rcgen::{
            BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
            KeyPair, KeyUsagePurpose,
        };
        let provider = Arc::new(aws_lc_rs::default_provider());
        // A day's validity, as the registry's tests give theirs: within what every platform's
        // verifier takes of a server's certificate.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let params = |names: Vec<String>| {
            let mut p = CertificateParams::new(names).unwrap();
            p.not_before = time::OffsetDateTime::from_unix_timestamp(now - 3600).unwrap();
            p.not_after = time::OffsetDateTime::from_unix_timestamp(now + 86400).unwrap();
            p
        };
        let ca = |name: &str| {
            let mut p = params(Vec::new());
            p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
            p.distinguished_name.push(DnType::CommonName, name);
            CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap()
        };
        let root = ca("shards test root");
        let stranger = ca("a CA no one trusts");
        let leaf = |issuer: &CertifiedIssuer<'static, KeyPair>| {
            let mut p = params(vec!["api.example.test".to_string()]);
            p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.signed_by(&KeyPair::generate().unwrap(), issuer)
                .unwrap()
                .der()
                .clone()
        };
        let v = verifier(vec![root.der().clone()], provider).unwrap();
        let name = ServerName::try_from("api.example.test").unwrap();
        assert!(
            v.verify_server_cert(&leaf(&root), &[], &name, &[], UnixTime::now())
                .is_ok()
        );
        assert!(
            v.verify_server_cert(&leaf(&stranger), &[], &name, &[], UnixTime::now())
                .is_err()
        );
        let other = ServerName::try_from("evil.example").unwrap();
        assert!(
            v.verify_server_cert(&leaf(&root), &[], &other, &[], UnixTime::now())
                .is_err()
        );
    }
}
