//! sigstore/sigstore's signature verifiers (pkg/signature, as buildx v0.37.1 vendors it):
//! a key loaded with a hash and options as LoadVerifierWithOpts loads it, the default
//! algorithm for a key (algorithm_registry.go), and VerifySignature for each kind with
//! ComputeDigestForVerifying's digests and each failure in its words.

use crate::x509::{Curve, Hash, PublicKey};

/// protobuf-specs' PublicKeyDetails, the rows the registry knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Details {
    RsaPkcs1v15_2048Sha256,
    RsaPkcs1v15_3072Sha256,
    RsaPkcs1v15_4096Sha256,
    RsaPss2048Sha256,
    RsaPss3072Sha256,
    RsaPss4096Sha256,
    EcdsaP256Sha256,
    EcdsaP384Sha384,
    EcdsaP384Sha256,
    EcdsaP521Sha512,
    EcdsaP521Sha256,
    Ed25519,
    Ed25519Ph,
}

impl Details {
    /// The enum's protobuf name and number.
    pub fn proto(self) -> (&'static str, i64) {
        match self {
            Details::RsaPkcs1v15_2048Sha256 => ("PKIX_RSA_PKCS1V15_2048_SHA256", 9),
            Details::RsaPkcs1v15_3072Sha256 => ("PKIX_RSA_PKCS1V15_3072_SHA256", 10),
            Details::RsaPkcs1v15_4096Sha256 => ("PKIX_RSA_PKCS1V15_4096_SHA256", 11),
            Details::RsaPss2048Sha256 => ("PKIX_RSA_PSS_2048_SHA256", 16),
            Details::RsaPss3072Sha256 => ("PKIX_RSA_PSS_3072_SHA256", 17),
            Details::RsaPss4096Sha256 => ("PKIX_RSA_PSS_4096_SHA256", 18),
            Details::EcdsaP256Sha256 => ("PKIX_ECDSA_P256_SHA_256", 5),
            Details::EcdsaP384Sha384 => ("PKIX_ECDSA_P384_SHA_384", 12),
            Details::EcdsaP384Sha256 => ("PKIX_ECDSA_P384_SHA_256", 19),
            Details::EcdsaP521Sha512 => ("PKIX_ECDSA_P521_SHA_512", 13),
            Details::EcdsaP521Sha256 => ("PKIX_ECDSA_P521_SHA_256", 20),
            Details::Ed25519 => ("PKIX_ED25519", 7),
            Details::Ed25519Ph => ("PKIX_ED25519_PH", 8),
        }
    }

    /// The registry's hash for the row (None: crypto.Hash(0)).
    pub fn hash(self) -> Option<Hash> {
        match self {
            Details::EcdsaP384Sha384 => Some(Hash::Sha384),
            Details::EcdsaP521Sha512 | Details::Ed25519Ph => Some(Hash::Sha512),
            Details::Ed25519 => None,
            _ => Some(Hash::Sha256),
        }
    }
}

/// The modulus's size in bits as rsa.PublicKey.Size()*8 counts it.
fn rsa_bits(n: &[u8]) -> usize {
    let i = n.iter().position(|x| *x != 0).unwrap_or(n.len());
    (n.len() - i) * 8
}

/// GetDefaultPublicKeyDetails.
pub fn default_details(key: &PublicKey, ed25519ph: bool, pss: bool) -> Result<Details, String> {
    let unsupported = || "unsupported public key type".to_string();
    match key {
        PublicKey::Rsa { n, .. } => match (rsa_bits(n), pss) {
            (2048, false) => Ok(Details::RsaPkcs1v15_2048Sha256),
            (3072, false) => Ok(Details::RsaPkcs1v15_3072Sha256),
            (4096, false) => Ok(Details::RsaPkcs1v15_4096Sha256),
            (2048, true) => Ok(Details::RsaPss2048Sha256),
            (3072, true) => Ok(Details::RsaPss3072Sha256),
            (4096, true) => Ok(Details::RsaPss4096Sha256),
            _ => Err(unsupported()),
        },
        PublicKey::Ecdsa { curve, .. } => match curve {
            Curve::P256 => Ok(Details::EcdsaP256Sha256),
            Curve::P384 => Ok(Details::EcdsaP384Sha384),
            Curve::P521 => Ok(Details::EcdsaP521Sha512),
            Curve::P224 => Err(unsupported()),
        },
        PublicKey::Ed25519(_) => Ok(if ed25519ph {
            Details::Ed25519Ph
        } else {
            Details::Ed25519
        }),
        _ => Err(unsupported()),
    }
}

/// A loaded verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verifier {
    /// PKCS #1 v1.5, or PSS with its salt length (None: found from the signature).
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
        hash: Hash,
        pss: Option<Option<usize>>,
    },
    Ecdsa {
        curve: Curve,
        point: Vec<u8>,
        hash: Hash,
    },
    Ed25519(Vec<u8>),
    Ed25519ph(Vec<u8>),
}

/// The options a key is loaded with.
#[derive(Debug, Clone, Copy, Default)]
pub struct Load {
    /// WithHash; SHA-256 where none is given. Some(None) is crypto.Hash(0).
    pub hash: Option<Option<Hash>>,
    pub ed25519ph: bool,
    /// WithRSAPSS and its salt length (None: auto).
    pub pss: Option<Option<usize>>,
}

const ECDSA_HASHES: [Hash; 4] = [Hash::Sha256, Hash::Sha512, Hash::Sha384, Hash::Sha224];
const ECDSA_VERIFY_HASHES: [Hash; 5] = [Hash::Sha256, Hash::Sha512, Hash::Sha384, Hash::Sha224, Hash::Sha1];
const RSA_HASHES: [Hash; 3] = [Hash::Sha256, Hash::Sha384, Hash::Sha512];
const RSA_VERIFY_HASHES: [Hash; 4] = [Hash::Sha1, Hash::Sha256, Hash::Sha384, Hash::Sha512];

/// LoadVerifierWithOpts.
pub fn load(key: &PublicKey, opts: Load) -> Result<Verifier, String> {
    let hash = opts.hash.unwrap_or(Some(Hash::Sha256));
    let supported = |list: &[Hash]| hash.is_some_and(|h| list.contains(&h));
    match key {
        PublicKey::Rsa { n, e } => {
            if !supported(&RSA_HASHES) {
                return Err("invalid hash function specified".into());
            }
            Ok(Verifier::Rsa {
                n: n.clone(),
                e: e.clone(),
                hash: hash.unwrap_or(Hash::Sha256),
                pss: opts.pss,
            })
        }
        PublicKey::Ecdsa { curve, point } => {
            if !supported(&ECDSA_HASHES) {
                return Err("invalid hash function specified".into());
            }
            Ok(Verifier::Ecdsa {
                curve: *curve,
                point: point.clone(),
                hash: hash.unwrap_or(Hash::Sha256),
            })
        }
        PublicKey::Ed25519(k) => Ok(if opts.ed25519ph {
            Verifier::Ed25519ph(k.clone())
        } else {
            Verifier::Ed25519(k.clone())
        }),
        _ => Err("unsupported public key type".into()),
    }
}

/// LoadVerifierFromAlgorithmDetails: the row's hash, the ED25519ph and RSA-PSS options
/// kept.
pub fn load_from_details(
    key: &PublicKey,
    details: Details,
    ed25519ph: bool,
    pss: Option<Option<usize>>,
) -> Result<Verifier, String> {
    load(
        key,
        Load {
            hash: Some(details.hash()),
            ed25519ph,
            pss,
        },
    )
}

/// LoadDefaultVerifier.
pub fn load_default(key: &PublicKey, ed25519ph: bool) -> Result<Verifier, String> {
    let details = default_details(key, ed25519ph, false)?;
    load_from_details(key, details, ed25519ph, None)
}

/// What VerifySignature is told (VerifyOptions): a digest in place of the message, and
/// the hash it was made with.
#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyWith<'a> {
    pub digest: Option<&'a [u8]>,
    pub hash: Option<Option<Hash>>,
}

fn hash_list(list: &[Option<Hash>]) -> String {
    let names: Vec<String> = list
        .iter()
        .map(|h| h.map_or_else(|| "unknown hash value 0".to_string(), |h| h.name().to_string()))
        .collect();
    format!("[{}]", names.join(" "))
}

/// ComputeDigestForVerifying.
fn digest_for_verifying(
    message: &[u8],
    default: Option<Hash>,
    supported: &[Option<Hash>],
    with: &VerifyWith<'_>,
) -> Result<Vec<u8>, String> {
    let hashed_with = with.hash.unwrap_or(default);
    if !supported.contains(&hashed_with) {
        return Err(format!(
            "unsupported hash algorithm: {} not in {}",
            shards_dockerfile::go::quote(hashed_with.map_or("unknown hash value 0", Hash::name).as_bytes()),
            hash_list(supported)
        ));
    }
    if let Some(d) = with.digest.filter(|d| !d.is_empty()) {
        if let Some(h) = hashed_with
            && d.len() != h.size()
        {
            return Err("unexpected length of digest for hash function specified".into());
        }
        return Ok(d.to_vec());
    }
    Ok(match hashed_with {
        Some(h) => h.of(message),
        None => message.to_vec(),
    })
}

/// Ed25519ph's verification by AWS-LC (RFC 8032 §5.1.7, phflag 1, no context).
fn ed25519ph(key: &[u8], digest: &[u8], sig: &[u8]) -> Result<(), String> {
    if sig.len() != 64 {
        return Err(format!("ed25519: bad signature length: {}", sig.len()));
    }
    let (Ok(k), Ok(d), Ok(s)) = (
        <[u8; 32]>::try_from(key),
        <[u8; 64]>::try_from(digest),
        <[u8; 64]>::try_from(sig),
    ) else {
        return Err(format!(
            "ed25519: bad Ed25519ph message hash length: {}",
            digest.len()
        ));
    };
    // SAFETY: each pointer is to a local array of the length AWS-LC reads (a 64-octet
    // digest and signature, a 32-octet key); the context is empty and not read.
    let ok = unsafe {
        aws_lc_sys::ED25519ph_verify_digest(d.as_ptr(), s.as_ptr(), k.as_ptr(), std::ptr::null(), 0)
    };
    if ok == 1 {
        Ok(())
    } else {
        Err("ed25519: invalid signature".into())
    }
}

/// encoding/asn1's reading of a signature as struct{ R, S *big.Int }: whether it parses.
fn asn1_rs(sig: &[u8]) -> bool {
    use crate::asn1::{Fields, Kind, Params, Value, unmarshal};
    let Ok((Value::Struct { inner, .. }, _)) = unmarshal(sig, Kind::Struct, &Params::default()) else {
        return false;
    };
    let mut f = Fields::new(inner);
    f.next(Kind::BigInt, &Params::default()).is_ok() && f.next(Kind::BigInt, &Params::default()).is_ok()
}

impl Verifier {
    /// VerifySignature.
    pub fn verify(&self, sig: &[u8], message: &[u8], with: &VerifyWith<'_>) -> Result<(), String> {
        match self {
            Verifier::Ecdsa { curve, point, hash } => {
                let supported: Vec<Option<Hash>> = ECDSA_VERIFY_HASHES.iter().copied().map(Some).collect();
                let digest = digest_for_verifying(message, Some(*hash), &supported, with)?;
                if asn1_rs(sig) {
                    if !crate::x509::ecdsa_asn1(*curve, point, &digest, sig) {
                        return Err("invalid signature when validating ASN.1 encoded signature".into());
                    }
                } else {
                    if sig.is_empty() || sig.len() > 132 || !sig.len().is_multiple_of(2) {
                        return Err("ecdsa: Invalid IEEE_P1363 encoded bytes".into());
                    }
                    let (r, s) = sig.split_at(sig.len() / 2);
                    let positive = |b: &[u8]| b.iter().any(|x| *x != 0);
                    if !positive(r)
                        || !positive(s)
                        || !crate::x509::ecdsa_verify(*curve, point, &digest, r, s)
                    {
                        return Err("invalid signature when validating IEEE_P1363 encoded signature".into());
                    }
                }
                Ok(())
            }
            Verifier::Rsa { n, e, hash, pss } => {
                let supported: Vec<Option<Hash>> = RSA_VERIFY_HASHES.iter().copied().map(Some).collect();
                let digest = digest_for_verifying(message, Some(*hash), &supported, with)?;
                let used = with.hash.unwrap_or(Some(*hash)).unwrap_or(*hash);
                if let Some(e) = shards_gitsign::arith::rsa_key_error(n, e) {
                    return Err(e);
                }
                let g = used.gitsign().ok_or("crypto/rsa: verification error")?;
                let ok = match pss {
                    None => shards_gitsign::arith::rsa_pkcs1_verify(n, e, g, &digest, sig),
                    Some(salt) => shards_gitsign::arith::rsa_pss_verify(n, e, g, &digest, sig, *salt),
                };
                ok.then_some(())
                    .ok_or_else(|| "crypto/rsa: verification error".to_string())
            }
            Verifier::Ed25519(k) => {
                // The options are ignored: the message is verified as given.
                let msg = digest_for_verifying(message, None, &[None], &VerifyWith::default())?;
                use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
                UnparsedPublicKey::new(&ED25519, k)
                    .verify(&msg, sig)
                    .map_err(|_| "failed to verify signature".to_string())
            }
            Verifier::Ed25519ph(k) => {
                let digest = digest_for_verifying(message, Some(Hash::Sha512), &[Some(Hash::Sha512)], with)?;
                ed25519ph(k, &digest, sig).map_err(|e| format!("failed to verify signature: {e}"))
            }
        }
    }

    /// The verifier's key, as PublicKey().
    pub fn public_key(&self) -> PublicKey {
        match self {
            Verifier::Rsa { n, e, .. } => PublicKey::Rsa {
                n: n.clone(),
                e: e.clone(),
            },
            Verifier::Ecdsa { curve, point, .. } => PublicKey::Ecdsa {
                curve: *curve,
                point: point.clone(),
            },
            Verifier::Ed25519(k) | Verifier::Ed25519ph(k) => PublicKey::Ed25519(k.clone()),
        }
    }

    pub fn is_ed25519(&self) -> bool {
        matches!(self, Verifier::Ed25519(_))
    }
}

/// x509.MarshalPKIXPublicKey of a key Go can marshal.
pub fn marshal_pkix(key: &PublicKey) -> Option<Vec<u8>> {
    use crate::der::{self, tlv};
    use crate::x509::{OID_EC, OID_ED25519, OID_P224, OID_P256, OID_P384, OID_P521, OID_RSA};
    let (alg, bits) = match key {
        PublicKey::Rsa { n, e } => {
            let int = |b: &[u8]| {
                let mut v = b.to_vec();
                if v.first().is_some_and(|x| x & 0x80 != 0) {
                    v.insert(0, 0);
                }
                tlv(der::INTEGER, &v)
            };
            let body = [int(n), int(e)].concat();
            (
                tlv(
                    der::SEQUENCE,
                    &[tlv(der::OID, OID_RSA), vec![0x05, 0x00]].concat(),
                ),
                tlv(der::SEQUENCE, &body),
            )
        }
        PublicKey::Ecdsa { curve, point } => {
            let named = match curve {
                Curve::P224 => OID_P224,
                Curve::P256 => OID_P256,
                Curve::P384 => OID_P384,
                Curve::P521 => OID_P521,
            };
            (
                tlv(
                    der::SEQUENCE,
                    &[tlv(der::OID, OID_EC), tlv(der::OID, named)].concat(),
                ),
                point.clone(),
            )
        }
        PublicKey::Ed25519(k) => (tlv(der::SEQUENCE, &tlv(der::OID, OID_ED25519)), k.clone()),
        _ => return None,
    };
    let mut bs = vec![0u8];
    bs.extend(bits);
    Some(tlv(der::SEQUENCE, &[alg, tlv(der::BIT_STRING, &bs)].concat()))
}
