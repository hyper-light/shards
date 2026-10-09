//! Sigstore's trusted root (trustroot/v1 TrustedRoot) as sigstore-go's root package reads
//! it (trusted_root.go): transparency logs and certificate transparency logs by their
//! key IDs in hex, Fulcio's certificate authorities and the timestamping authorities,
//! each with the time it is trusted for.

use std::collections::BTreeMap;

use crate::time::Time;
use crate::x509::{Certificate, Hash, PublicKey};

/// A transparency log (root.TransparencyLog), Rekor's or a CT log's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparencyLog {
    pub base_url: String,
    pub id: Vec<u8>,
    /// ValidityPeriodStart; None the zero Time.
    pub start: Option<Time>,
    /// ValidityPeriodEnd; None the zero Time.
    pub end: Option<Time>,
    pub hash: Hash,
    pub key: PublicKey,
    /// SignatureHashFunc: the key's (getSignatureHashAlgo).
    pub signature_hash: Hash,
}

/// A Fulcio certificate authority (root.FulcioCertificateAuthority).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateAuthority {
    pub uri: String,
    pub root: Certificate,
    pub intermediates: Vec<Certificate>,
    pub start: Option<Time>,
    pub end: Option<Time>,
}

/// A timestamping authority (root.SigstoreTimestampingAuthority).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampingAuthority {
    pub uri: String,
    pub root: Option<Certificate>,
    pub intermediates: Vec<Certificate>,
    pub leaf: Option<Certificate>,
    pub start: Option<Time>,
    pub end: Option<Time>,
}

/// A trusted root (root.TrustedRoot).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedRoot {
    pub tlogs: BTreeMap<String, TransparencyLog>,
    pub certificate_authorities: Vec<CertificateAuthority>,
    pub ctlogs: BTreeMap<String, TransparencyLog>,
    pub timestamp_authorities: Vec<TimestampingAuthority>,
}

/// A verified timestamp (root.Timestamp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestamp {
    pub time: Time,
    pub uri: String,
}

use crate::Error;
use crate::proto::{self, Msg, Val};
use crate::schemas::{self, HASH_ALGORITHM, PUBLIC_KEY_DETAILS, enum_name};
use crate::x509::Curve;

/// TrustedRootMediaType01.
pub const MEDIA_TYPE: &str = "application/vnd.dev.sigstore.trustedroot+json;version=0.1";

/// NewTrustedRootFromJSON.
pub fn parse(json: &[u8]) -> Result<TrustedRoot, Error> {
    let m = proto::unmarshal(json, &schemas::TRUSTED_ROOT)
        .map_err(|e| Error(format!("failed to proto-json unmarshal trusted root: {e}")))?;
    let media = m.string(1);
    if media != MEDIA_TYPE {
        return Err(Error(format!("unsupported TrustedRoot media type: {media}")));
    }
    let msgs = |n: u32| -> Vec<&Msg> {
        m.list(n)
            .iter()
            .filter_map(|v| match v {
                Val::Msg(x) => Some(x),
                _ => None,
            })
            .collect()
    };
    let tlogs = transparency_logs(&msgs(2))?;
    let mut cas = Vec::new();
    for ca in msgs(3) {
        cas.push(certificate_authority(ca)?);
    }
    let mut tsas = Vec::new();
    for ca in msgs(5) {
        tsas.push(timestamping_authority(ca)?);
    }
    let ctlogs = transparency_logs(&msgs(4))?;
    Ok(TrustedRoot {
        tlogs,
        certificate_authorities: cas,
        ctlogs,
        timestamp_authorities: tsas,
    })
}

/// TimeRange's start and end, as AsTime gives them (UTC).
fn time_range(m: &Msg) -> (Option<Time>, Option<Time>) {
    let t = |n: u32| m.time(n).map(|(s, ns)| Time::utc(s, ns));
    (t(1), t(2))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// getSignatureHashAlgo.
pub fn signature_hash(key: &PublicKey) -> Hash {
    match key {
        PublicKey::Ecdsa {
            curve: Curve::P384, ..
        } => Hash::Sha384,
        PublicKey::Ecdsa {
            curve: Curve::P521, ..
        }
        | PublicKey::Ed25519(_) => Hash::Sha512,
        _ => Hash::Sha256,
    }
}

/// ParseTransparencyLogs.
fn transparency_logs(tlogs: &[&Msg]) -> Result<BTreeMap<String, TransparencyLog>, Error> {
    let mut out = BTreeMap::new();
    for tlog in tlogs {
        let alg = tlog.enumeration(2);
        if alg != 1 {
            return Err(Error(format!(
                "unsupported tlog hash algorithm: {}",
                enum_name(HASH_ALGORITHM, alg)
            )));
        }
        let log_id = tlog.msg(4).ok_or_else(|| Error("tlog missing log ID".into()))?;
        let key_id = log_id
            .bytes(1)
            .ok_or_else(|| Error("tlog missing log ID key ID".into()))?;
        let pk = tlog
            .msg(3)
            .ok_or_else(|| Error("tlog missing public key".into()))?;
        let raw = pk
            .bytes(1)
            .ok_or_else(|| Error("tlog missing public key raw bytes".into()))?;
        let base_url = tlog.string(1);
        let details = pk.enumeration(2);
        let parse_failed =
            |e: crate::x509::X509Error| Error(format!("failed to parse public key for tlog: {base_url} {e}"));
        let name = enum_name(PUBLIC_KEY_DETAILS, details);
        let key = match details {
            5 | 12 | 13 => {
                let k = crate::x509::parse_pkix_public_key(&raw).map_err(parse_failed)?;
                if !matches!(k, PublicKey::Ecdsa { .. }) {
                    return Err(Error(format!("tlog public key is not ECDSA: {name}")));
                }
                k
            }
            9..=11 => {
                let k = crate::x509::parse_pkix_public_key(&raw).map_err(parse_failed)?;
                if !matches!(k, PublicKey::Rsa { .. }) {
                    return Err(Error(format!("tlog public key is not RSA: {name}")));
                }
                k
            }
            7 => {
                let k = crate::x509::parse_pkix_public_key(&raw).map_err(parse_failed)?;
                if !matches!(k, PublicKey::Ed25519(_)) {
                    // sigstore-go's words for this case, as it has them.
                    return Err(Error(format!("tlog public key is not RSA: {name}")));
                }
                k
            }
            1 => crate::x509::parse_pkcs1_public_key(&raw).map_err(parse_failed)?,
            _ => return Err(Error(format!("unsupported tlog public key type: {name}"))),
        };
        let valid_for = pk
            .msg(3)
            .ok_or_else(|| Error("tlog missing public key validity period".into()))?;
        let (start, end) = time_range(valid_for);
        if start.is_none() {
            return Err(Error("tlog missing public key validity period start time".into()));
        }
        out.insert(
            hex(&key_id),
            TransparencyLog {
                base_url,
                id: key_id,
                start,
                end,
                hash: Hash::Sha256,
                signature_hash: signature_hash(&key),
                key,
            },
        );
    }
    Ok(out)
}

/// The chain of a CertificateAuthority, each certificate parsed.
fn chain(ca: &Msg) -> Result<Vec<crate::x509::Certificate>, Error> {
    let chain = ca
        .msg(3)
        .ok_or_else(|| Error("CertificateAuthority missing cert chain".into()))?;
    let certs = chain.list(1);
    if certs.is_empty() {
        return Err(Error("CertificateAuthority cert chain is empty".into()));
    }
    let uri = ca.string(2);
    let mut out = Vec::new();
    for c in certs {
        let raw = match c {
            Val::Msg(m) => m.bytes(1).unwrap_or_default(),
            _ => Vec::new(),
        };
        out.push(
            crate::x509::Certificate::parse(&raw)
                .map_err(|e| Error(format!("failed to parse certificate for {uri} {e}")))?,
        );
    }
    Ok(out)
}

/// ParseCertificateAuthority.
fn certificate_authority(ca: &Msg) -> Result<CertificateAuthority, Error> {
    let mut certs = chain(ca)?;
    let root = certs
        .pop()
        .ok_or_else(|| Error("CertificateAuthority cert chain is empty".into()))?;
    let (start, end) = ca.msg(4).map(time_range).unwrap_or_default();
    Ok(CertificateAuthority {
        uri: ca.string(2),
        root,
        intermediates: certs,
        start,
        end,
    })
}

/// ParseTimestampingAuthority.
fn timestamping_authority(ca: &Msg) -> Result<TimestampingAuthority, Error> {
    let certs = chain(ca)?;
    let n = certs.len();
    let mut tsa = TimestampingAuthority {
        uri: ca.string(2),
        root: None,
        intermediates: Vec::new(),
        leaf: None,
        start: None,
        end: None,
    };
    for (i, c) in certs.into_iter().enumerate() {
        if i == 0 && !c.is_ca {
            tsa.leaf = Some(c);
        } else if i + 1 < n {
            tsa.intermediates.push(c);
        } else {
            tsa.root = Some(c);
        }
    }
    let (start, end) = ca.msg(4).map(time_range).unwrap_or_default();
    tsa.start = start;
    tsa.end = end;
    Ok(tsa)
}
