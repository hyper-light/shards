//! A run's own CA (D242). Its key is made as the network process starts, before it confines
//! itself, for a run its spawner says has credential grants (`--credentials`), and never
//! leaves the process. Its certificate is signed once the run host gives the grants
//! (`NET_CREDENTIALS`), constrained to their hosts, and answered to the run host for the
//! members' trust.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, NameConstraints, PKCS_ECDSA_P256_SHA256,
};

/// The CA's key, made before the process confines itself. AWS-LC's first draw from its
/// random generator opens the VM-generation device (crypto/ube/vm_ube_detect.c) and marks a
/// page MADV_WIPEONFORK (crypto/ube/fork_ube_detect.c), once per process, and the network
/// process's seccomp filter, which ends the process at any other call, allows neither
/// (confine.rs). Every later draw (the certificate's signature, its leaves) reads
/// what that first one set up.
pub struct Key(KeyPair);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    /// ECDSA over P-256 with SHA-256: the signature every TLS 1.3 client must verify
    /// (RFC 8446 §9.1), so every harness's TLS stack takes the CA's certificates.
    pub fn generate() -> Result<Key, String> {
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map(Key)
            .map_err(|e| format!("the run's CA key: {e}"))
    }
}

/// A run's CA: a self-signed root, the trust anchor its members are given, whose
/// certificates may name the run's granted hosts alone.
pub struct Ca {
    issuer: Issuer<'static, KeyPair>,
    der: Vec<u8>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ca")
            .field("certificate", &self.der.len())
            .finish_non_exhaustive()
    }
}

/// Valid from 1970-01-01T00:00:00Z, the Unix epoch, the first instant every harness's TLS
/// stack reads in a certificate (rustls-webpki refuses an earlier one as a bad time: "We
/// don't support dates before January 1, 1970", time.rs), to 99991231235959Z, the value of
/// no well-defined end (RFC 5280 §4.1.2.5): no clock a guest may read falls outside it, and
/// the CA's key, which dies with the process, bounds the life of what it signs instead of a
/// date.
fn validity(params: &mut CertificateParams) -> Result<(), String> {
    let range = |e: time::error::ComponentRange| format!("a certificate's validity: {e}");
    params.not_before = time::Date::from_calendar_date(1970, time::Month::January, 1)
        .map_err(range)?
        .midnight()
        .assume_utc();
    params.not_after = time::Date::from_calendar_date(9999, time::Month::December, 31)
        .map_err(range)?
        .with_hms(23, 59, 59)
        .map_err(range)?
        .assume_utc();
    Ok(())
}

impl Ca {
    /// The CA for `hosts`, each a DNS name a credential grant names, signed with `key`.
    ///
    /// - Name constraints, dNSName subtrees alone: a certificate below it names those hosts
    ///   or names under them (RFC 5280 §4.2.1.10). Every TLS stack a harness uses holds a
    ///   trust anchor to its constraints, and rustls-webpki refuses an anchor's directory-name
    ///   constraints (plan research f37, f38).
    /// - A path of no CA below it (`pathLenConstraint` 0): it signs leaves alone.
    /// - Valid for [`validity`]'s range.
    pub fn new(key: Key, hosts: &[&str]) -> Result<Ca, String> {
        let err = |e: rcgen::Error| format!("the run's CA: {e}");
        if hosts.is_empty() {
            // A name constraint names at least one subtree (RFC 5280 §4.2.1.10,
            // GeneralSubtrees SIZE (1..MAX)), and a CA of no host could sign for none.
            return Err("the run's CA: no host is granted a credential".into());
        }
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "shards microVM credentials");
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: hosts
                .iter()
                .map(|h| GeneralSubtree::DnsName((*h).to_string()))
                .collect(),
            excluded_subtrees: Vec::new(),
        });
        validity(&mut params)?;
        let cert = params.self_signed(&key.0).map_err(err)?;
        Ok(Ca {
            der: cert.der().to_vec(),
            issuer: Issuer::new(params, key.0),
        })
    }

    /// Its certificate, DER.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// A leaf for the server `name`, signed by the CA with a key of its own, as the injector
    /// answers a guest's TLS to a granted host: its certificate and its PKCS #8 key, DER.
    /// One for a name the CA's constraints do not admit is signed, and no client takes it.
    pub fn leaf(&self, name: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
        let err = |e: rcgen::Error| format!("a leaf for {name}: {e}");
        let mut params = CertificateParams::new(vec![name.to_string()]).map_err(err)?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        validity(&mut params)?;
        let key = Key::generate()?;
        let cert = params.signed_by(&key.0, &self.issuer).map_err(err)?;
        Ok((cert.der().to_vec(), key.0.serialize_der()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::sync::Arc;

    /// The CA's leaf for `name`, as the injector makes one per SNI.
    fn leaf(ca: &Ca, name: &str) -> CertificateDer<'static> {
        CertificateDer::from(ca.leaf(name).unwrap().0)
    }

    /// Whether a client that trusts `ca` alone takes `cert` as `name`'s, as rustls does.
    fn verifies(ca: &Ca, cert: &CertificateDer<'_>, name: &str) -> bool {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .unwrap();
        let name = ServerName::try_from(name.to_string()).unwrap();
        verifier
            .verify_server_cert(cert, &[], &name, &[], UnixTime::now())
            .is_ok()
    }

    /// A leaf the run's CA signs is trusted for a granted host and for nothing else: its
    /// name constraints hold a certificate for another host to no client, whatever the CA
    /// signs (R10: without them, the ungranted leaf verifies and this fails).
    #[test]
    fn its_leaves_hold_for_granted_hosts_alone() {
        let ca = Ca::new(Key::generate().unwrap(), &["api.anthropic.com", "chatgpt.com"]).unwrap();
        for host in ["api.anthropic.com", "chatgpt.com"] {
            assert!(verifies(&ca, &leaf(&ca, host), host), "{host}'s leaf was refused");
        }
        for host in ["evil.example", "anthropic.com", "chatgpt.com.evil.example"] {
            assert!(
                !verifies(&ca, &leaf(&ca, host), host),
                "{host}'s leaf held, though no grant names it"
            );
        }
    }

    /// The CA and its leaves are valid from the Unix epoch (a UTCTime) to 99991231235959Z (a
    /// GeneralizedTime), as written in their certificates: a leaf of a time before the epoch
    /// is one rustls refuses as badly encoded, as this test's first run showed.
    #[test]
    fn it_is_valid_from_the_epoch_to_no_end() {
        let ca = Ca::new(Key::generate().unwrap(), &["api.anthropic.com"]).unwrap();
        for der in [ca.der().to_vec(), ca.leaf("api.anthropic.com").unwrap().0] {
            let has = |bytes: &[u8]| der.windows(bytes.len()).any(|w| w == bytes);
            assert!(has(b"\x17\x0d700101000000Z"), "notBefore is not the Unix epoch");
            assert!(has(b"\x18\x0f99991231235959Z"), "notAfter is not 99991231235959Z");
        }
    }

    /// A CA of no host is refused: it could sign for none, and an empty constraint is no
    /// constraint X.509 allows.
    #[test]
    fn no_host_no_ca() {
        assert!(Ca::new(Key::generate().unwrap(), &[]).is_err());
    }
}
