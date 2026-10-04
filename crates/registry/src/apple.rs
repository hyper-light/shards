//! macOS's verifier: the system's trust (shards_apple::trust) behind rustls, refusing
//! as rustls-platform-verifier 0.7.1 refuses (src/verification/apple.rs), with the same
//! errors. It differs from it in one way: Security is loaded the first time a certificate
//! is verified, not as the process starts, so the commands that never reach a registry
//! never pay for it (PM M113).

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, OtherError, SignatureScheme};
use shards_apple::trust::{self, Refusal};

/// The platform verifier's error for an end-entity certificate without server
/// authentication among its extended key usages, in its words.
#[derive(Debug)]
pub(crate) struct EkuError;

impl std::fmt::Display for EkuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("certificate had invalid extensions")
    }
}

impl std::error::Error for EkuError {}

fn invalid_certificate(reason: String) -> TlsError {
    let reason: Box<dyn std::error::Error + Send + Sync> = Box::from(reason);
    TlsError::InvalidCertificate(CertificateError::Other(OtherError(Arc::from(reason))))
}

#[derive(Debug)]
pub struct Verifier {
    extra_roots: Vec<CertificateDer<'static>>,
    provider: Arc<CryptoProvider>,
}

impl Verifier {
    pub fn new_with_extra_roots(
        extra_roots: impl IntoIterator<Item = CertificateDer<'static>>,
        provider: Arc<CryptoProvider>,
    ) -> Result<Verifier, TlsError> {
        Ok(Verifier {
            extra_roots: extra_roots.into_iter().collect(),
            provider,
        })
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // An IP address as its text, which the leaf must name; a trailing dot left off.
        let server = server_name.to_str();
        let server = server.strip_suffix('.').unwrap_or(&server);
        let chain: Vec<&[u8]> = std::iter::once(end_entity.as_ref())
            .chain(intermediates.iter().map(|c| c.as_ref()))
            .collect();
        let roots: Vec<&[u8]> = self.extra_roots.iter().map(|c| c.as_ref()).collect();
        let ocsp = (!ocsp_response.is_empty()).then_some(ocsp_response);
        match trust::evaluate(&chain, server, ocsp, now.as_secs(), &roots) {
            Ok(()) => Ok(ServerCertVerified::assertion()),
            Err(refusal) => Err(match refusal {
                Refusal::BadEncoding => TlsError::InvalidCertificate(CertificateError::BadEncoding),
                Refusal::NotValidForName => TlsError::InvalidCertificate(CertificateError::NotValidForName),
                Refusal::UnknownIssuer => TlsError::InvalidCertificate(CertificateError::UnknownIssuer),
                Refusal::ExtendedKeyUsage => {
                    TlsError::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(EkuError))))
                }
                Refusal::Revoked => TlsError::InvalidCertificate(CertificateError::Revoked),
                Refusal::FailedToGetCurrentTime => TlsError::FailedToGetCurrentTime,
                Refusal::Invalid(why) => invalid_certificate(why),
                Refusal::General(why) | Refusal::Unavailable(why) => TlsError::General(why),
            }),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::crypto::aws_lc_rs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn params(sans: Vec<String>, from: i64, to: i64) -> CertificateParams {
        let mut p = CertificateParams::new(sans).unwrap();
        p.not_before = time::OffsetDateTime::from_unix_timestamp(from).unwrap();
        p.not_after = time::OffsetDateTime::from_unix_timestamp(to).unwrap();
        p
    }

    fn ca(
        name: &str,
        issuer: Option<&CertifiedIssuer<'static, KeyPair>>,
    ) -> CertifiedIssuer<'static, KeyPair> {
        let mut p = params(Vec::new(), now() - 3600, now() + 86400);
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        p.distinguished_name.push(DnType::CommonName, name);
        match issuer {
            None => CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap(),
            Some(i) => CertifiedIssuer::signed_by(p, KeyPair::generate().unwrap(), i).unwrap(),
        }
    }

    fn leaf(
        sans: &[&str],
        usage: ExtendedKeyUsagePurpose,
        from: i64,
        to: i64,
        issuer: &CertifiedIssuer<'static, KeyPair>,
    ) -> CertificateDer<'static> {
        let mut p = params(sans.iter().map(|s| s.to_string()).collect(), from, to);
        p.extended_key_usages = vec![usage];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.signed_by(&KeyPair::generate().unwrap(), issuer)
            .unwrap()
            .der()
            .clone()
    }

    /// What one verifier makes of a case, as text: the verdict, and an error's own words.
    fn verdict(v: &dyn ServerCertVerifier, chain: &[CertificateDer<'static>], name: &str, at: u64) -> String {
        let name = ServerName::try_from(name.to_string()).unwrap();
        match v.verify_server_cert(
            &chain[0],
            &chain[1..],
            &name,
            &[],
            UnixTime::since_unix_epoch(std::time::Duration::from_secs(at)),
        ) {
            Ok(_) => "trusted".into(),
            Err(e) => format!("{e:?} / {e}"),
        }
    }

    /// Every case judged by this verifier and by rustls-platform-verifier, which it
    /// replaces: the same verdict, in the same words.
    #[test]
    fn it_trusts_and_refuses_as_the_platform_verifier_does() {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let root = ca("shards test root", None);
        let intermediate = ca("shards test intermediate", Some(&root));
        let stranger = ca("a CA no one trusts", None);
        let t = now();
        let server = ExtendedKeyUsagePurpose::ServerAuth;
        let cases: Vec<(&str, Vec<CertificateDer<'static>>, &str, u64)> = vec![
            (
                "valid",
                vec![leaf(&["localhost"], server.clone(), t - 3600, t + 86400, &root)],
                "localhost",
                t as u64,
            ),
            (
                "trailing dot",
                vec![leaf(&["localhost"], server.clone(), t - 3600, t + 86400, &root)],
                "localhost.",
                t as u64,
            ),
            (
                "wrong host",
                vec![leaf(&["localhost"], server.clone(), t - 3600, t + 86400, &root)],
                "example.com",
                t as u64,
            ),
            (
                "unknown CA",
                vec![leaf(
                    &["localhost"],
                    server.clone(),
                    t - 3600,
                    t + 86400,
                    &stranger,
                )],
                "localhost",
                t as u64,
            ),
            (
                "expired",
                vec![leaf(&["localhost"], server.clone(), t - 7200, t - 3600, &root)],
                "localhost",
                t as u64,
            ),
            (
                "client only",
                vec![leaf(
                    &["localhost"],
                    ExtendedKeyUsagePurpose::ClientAuth,
                    t - 3600,
                    t + 86400,
                    &root,
                )],
                "localhost",
                t as u64,
            ),
            (
                "IP address",
                vec![leaf(&["127.0.0.1"], server.clone(), t - 3600, t + 86400, &root)],
                "127.0.0.1",
                t as u64,
            ),
            (
                "via intermediate",
                vec![
                    leaf(&["localhost"], server.clone(), t - 3600, t + 86400, &intermediate),
                    intermediate.der().clone(),
                ],
                "localhost",
                t as u64,
            ),
            (
                "intermediate missing",
                vec![leaf(
                    &["localhost"],
                    server.clone(),
                    t - 3600,
                    t + 86400,
                    &intermediate,
                )],
                "localhost",
                t as u64,
            ),
            (
                "not DER",
                vec![CertificateDer::from(vec![0x30, 0x03, 0x01, 0x02, 0x03])],
                "localhost",
                t as u64,
            ),
            (
                "before Apple's epoch",
                vec![leaf(&["localhost"], server.clone(), t - 3600, t + 86400, &root)],
                "localhost",
                1000,
            ),
        ];
        let ours = Verifier::new_with_extra_roots(vec![root.der().clone()], provider.clone()).unwrap();
        let theirs =
            rustls_platform_verifier::Verifier::new_with_extra_roots(vec![root.der().clone()], provider)
                .unwrap();
        for (what, chain, name, at) in &cases {
            let (a, b) = (
                verdict(&ours, chain, name, *at),
                verdict(&theirs, chain, name, *at),
            );
            assert_eq!(a, b, "{what}");
        }
        // And they are not all one verdict: the cases tell trust from refusal.
        let first = verdict(&ours, &cases[0].1, cases[0].2, cases[0].3);
        let third = verdict(&ours, &cases[2].1, cases[2].2, cases[2].3);
        assert_eq!(first, "trusted");
        assert_ne!(third, "trusted");
    }
}
