//! The build's proxy's certificates (D110): a CA of the build's own, which each step under
//! the proxy trusts while it runs, and a certificate for each host a step tunnels to,
//! signed by it, as BuildKit v0.33.0's proxy makes them (`newCA`, `certForHost`): the CA's
//! name `BuildKit exec proxy`, serial 1, valid from an hour ago for ten years, for signing
//! certificates; a host's its name (an IP address's as one), for serving TLS, valid from
//! an hour ago for a day and made again an hour before it ends, at most 1024 kept, the
//! least recently used going first. The keys are P-256 where BuildKit's are RSA-2048 (PM
//! M150), and the CA is the build's, its key never leaving this process, where BuildKit's
//! daemon makes one as it starts and keeps it for every build.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const HOUR: Duration = Duration::from_secs(3600);
const CA_LIFE: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
const LEAF_LIFE: Duration = Duration::from_secs(24 * 3600);
/// The most hosts' certificates kept (`proxyCertCacheMaxEntries`).
const LEAVES: usize = 1024;

/// The proxy's CA, and the TLS each host it has served is served with.
pub struct Authority {
    pem: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    leaves: Mutex<Leaves>,
}

#[derive(Default)]
struct Leaves {
    by_host: HashMap<String, Leaf>,
    /// A count of uses, for the least recently used.
    tick: u64,
}

struct Leaf {
    config: Arc<ServerConfig>,
    /// When it is made again: an hour before it ends.
    renew: Instant,
    used: u64,
}

fn err(what: &str) -> impl Fn(rcgen::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Its validity: from an hour ago for `life`.
fn validity(params: &mut CertificateParams, life: Duration) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - HOUR;
    params.not_after = now + life;
}

impl Authority {
    /// A CA of its own.
    pub fn new() -> Result<Authority, String> {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(err("the proxy's CA key"))?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "BuildKit exec proxy");
        params.serial_number = Some(SerialNumber::from_slice(&[1]));
        validity(&mut params, CA_LIFE);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).map_err(err("the proxy's CA"))?;
        Ok(Authority {
            pem: cert.pem().into_bytes(),
            issuer: Issuer::new(params, key),
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
            leaves: Mutex::new(Leaves::default()),
        })
    }

    /// The CA's certificate, PEM, as a step's trust bundle takes it.
    pub fn pem(&self) -> &[u8] {
        &self.pem
    }

    /// `certForHost`: the TLS a tunnel to `host` is served with, its certificate made now
    /// or kept from before. HTTP/1.1 alone is offered (ALPN), as BuildKit offers it.
    pub fn config_for(&self, host: &str) -> Result<Arc<ServerConfig>, String> {
        let mut leaves = self.leaves.lock().unwrap_or_else(PoisonError::into_inner);
        leaves.tick += 1;
        let tick = leaves.tick;
        let now = Instant::now();
        if let Some(leaf) = leaves.by_host.get_mut(host)
            && now < leaf.renew
        {
            leaf.used = tick;
            return Ok(leaf.config.clone());
        }
        let config = Arc::new(self.leaf(host)?);
        leaves.by_host.insert(
            host.to_string(),
            Leaf {
                config: config.clone(),
                renew: now + (LEAF_LIFE - HOUR),
                used: tick,
            },
        );
        while leaves.by_host.len() > LEAVES {
            let Some(oldest) = leaves
                .by_host
                .iter()
                .min_by_key(|(_, l)| l.used)
                .map(|(h, _)| h.clone())
            else {
                break;
            };
            leaves.by_host.remove(&oldest);
        }
        Ok(config)
    }

    /// A certificate for `host`, an IP address's or a name's, and the TLS that serves it.
    fn leaf(&self, host: &str) -> Result<ServerConfig, String> {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(err("a host's key"))?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host);
        params.subject_alt_names = vec![match host.parse::<std::net::IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(host.try_into().map_err(err("a host's name"))?),
        }];
        validity(&mut params, LEAF_LIFE);
        // A P-256 key signs alone: no key encipherment (RFC 5480 §3).
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params
            .signed_by(&key, &self.issuer)
            .map_err(err("a host's certificate"))?;
        let chain = vec![CertificateDer::from(cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = ServerConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|e| format!("a host's TLS: {e}"))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| format!("a host's TLS: {e}"))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }

    /// How many hosts' certificates are kept.
    #[cfg(test)]
    fn kept(&self) -> usize {
        self.leaves
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_host
            .len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// Says hello over TLS to `config`'s server for `name`, trusting `ca` alone.
    fn hello(config: Arc<ServerConfig>, ca: &[u8], name: &str) -> Result<Vec<u8>, String> {
        let (a, b) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(config).unwrap(), b);
            let mut got = [0u8; 5];
            if tls.read_exact(&mut got).is_ok() {
                let _ = tls.write_all(b"world");
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls::pki_types::pem::PemObject::pem_slice_iter(ca) {
            roots
                .add(c.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        }
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(client), name).map_err(|e| e.to_string())?;
        let mut tls = rustls::StreamOwned::new(conn, a);
        let said = tls.write_all(b"hello").and_then(|()| {
            let mut got = Vec::new();
            tls.read_to_end(&mut got).map(|_| got)
        });
        drop(tls);
        let _ = server.join();
        said.map_err(|e| e.to_string())
    }

    /// A host's certificate chains to the CA, for its name or its address, and to no
    /// other; it is kept for the next tunnel to that host.
    #[test]
    fn tunnels_are_served_with_a_certificate_the_ca_signed() {
        let a = Authority::new().unwrap();
        assert!(a.pem().starts_with(b"-----BEGIN CERTIFICATE-----"));
        assert_eq!(
            hello(a.config_for("example.com").unwrap(), a.pem(), "example.com").unwrap(),
            b"world"
        );
        assert_eq!(
            hello(a.config_for("10.0.0.1").unwrap(), a.pem(), "10.0.0.1").unwrap(),
            b"world"
        );
        assert!(Arc::ptr_eq(
            &a.config_for("example.com").unwrap(),
            &a.config_for("example.com").unwrap()
        ));
        // Another host's name, another CA: refused.
        assert!(hello(a.config_for("example.com").unwrap(), a.pem(), "other.example").is_err());
        let other = Authority::new().unwrap();
        assert!(hello(a.config_for("example.com").unwrap(), other.pem(), "example.com").is_err());
        // A name no certificate can carry.
        assert!(a.config_for("bad name\u{e9}").is_err());
    }

    /// At most 1024 kept, the least recently used going first.
    #[test]
    fn the_least_recently_used_certificate_goes_first() {
        let a = Authority::new().unwrap();
        let first = a.config_for("h0.example").unwrap();
        for i in 1..=LEAVES {
            a.config_for(&format!("h{i}.example")).unwrap();
            if i == 512 {
                // Used again: not the oldest any more.
                a.config_for("h0.example").unwrap();
            }
        }
        assert_eq!(a.kept(), LEAVES);
        assert!(Arc::ptr_eq(&first, &a.config_for("h0.example").unwrap()));
        assert!(!a.leaves.lock().unwrap().by_host.contains_key("h1.example"));
    }

    /// The certificates as BuildKit makes them (`newCA`, `certForHost`): RSA-2048 keys, a
    /// leaf with key encipherment, each made the same way here but for its key.
    fn rsa_ca() -> (Issuer<'static, KeyPair>, Vec<u8>) {
        let key = KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048).unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "BuildKit exec proxy");
        params.serial_number = Some(SerialNumber::from_slice(&[1]));
        validity(&mut params, CA_LIFE);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let der = params.self_signed(&key).unwrap().der().to_vec();
        (Issuer::new(params, key), der)
    }

    fn rsa_leaf(issuer: &Issuer<'static, KeyPair>, host: &str) -> ServerConfig {
        let key = KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048).unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host);
        params.subject_alt_names = vec![SanType::DnsName(host.try_into().unwrap())];
        validity(&mut params, LEAF_LIFE);
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, issuer).unwrap();
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(cert.der().to_vec())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                )
                .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        config
    }

    /// This thread's CPU time, in microseconds.
    fn cpu_us() -> u128 {
        // SAFETY: a zeroed timespec for clock_gettime(2) to fill.
        let mut t: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: clock_gettime(2) of this thread's CPU clock.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
        u128::try_from(t.tv_sec).unwrap_or(0) * 1_000_000 + u128::try_from(t.tv_nsec).unwrap_or(0) / 1000
    }

    /// PM M150: what the proxy's certificates cost to make, P-256 (shards) against
    /// RSA-2048 (BuildKit), each kind made in turn so a busy host weighs on both alike: a
    /// CA (a build's, as its first step under the proxy starts), a host's certificate and
    /// its TLS (a tunnel's first to that host), and one kept (every tunnel after). The
    /// thread's own CPU time beside the wall's says what of a tail the host's scheduler
    /// made.
    #[test]
    #[ignore = "a measurement: docs/research/measurements/build-proxy/run.sh"]
    fn certificate_costs() {
        let quantiles = |mut took: Vec<u128>| {
            took.sort_unstable();
            let q = |p: usize| took[(took.len() * p / 100).min(took.len() - 1)];
            format!(
                "p50 {} p90 {} p99 {} max {}",
                q(50),
                q(90),
                q(99),
                took[took.len() - 1]
            )
        };
        let n = 200;
        let names = [
            "CA, P-256",
            "CA, RSA-2048",
            "host certificate and TLS, P-256",
            "host certificate and TLS, RSA-2048",
            "host certificate kept",
        ];
        let mut wall: Vec<Vec<u128>> = vec![Vec::new(); names.len()];
        let mut cpu: Vec<Vec<u128>> = vec![Vec::new(); names.len()];
        let authority = Authority::new().unwrap();
        let (rsa, _) = rsa_ca();
        authority.config_for("kept.example").unwrap();
        for i in 0..n {
            let host = format!("h{i}.example");
            let mut work: [Box<dyn FnMut()>; 5] = [
                Box::new(|| drop(Authority::new().unwrap())),
                Box::new(|| drop(rsa_ca())),
                Box::new(|| drop(authority.leaf(&host).unwrap())),
                Box::new(|| drop(rsa_leaf(&rsa, &host))),
                Box::new(|| drop(authority.config_for("kept.example").unwrap())),
            ];
            for (k, f) in work.iter_mut().enumerate() {
                let (t, c) = (Instant::now(), cpu_us());
                f();
                wall[k].push(t.elapsed().as_micros());
                cpu[k].push(cpu_us() - c);
            }
        }
        for (k, name) in names.iter().enumerate() {
            println!("{name}: n={n} wall {}", quantiles(wall[k].clone()));
            println!("{name}: n={n} cpu {}", quantiles(cpu[k].clone()));
        }
    }
}
