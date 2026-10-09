//! TUF keys as go-tuf reads them (metadata/keys.go ToPublicKey) and verifies with them
//! (sigstore's signature verifiers): ECDSA keys (`ecdsa` and `ecdsa-sha2-nistp256`) and
//! RSA ones as PEM public keys, Ed25519 ones in hex; ECDSA and Ed25519 checked through
//! AWS-LC, RSASSA-PSS (salt length found, as Go's PSSSaltLengthAuto) through
//! `shards_gitsign::arith`.

use aws_lc_rs::digest;

use crate::Error;
use crate::metadata::Key;

/// A curve go-tuf's ECDSA keys may be on, as x509 parses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    P224,
    P256,
    P384,
    P521,
}

/// A public key ToPublicKey returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    Ecdsa { curve: Curve, point: Vec<u8> },
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ed25519(Vec<u8>),
}

/// The hash a key's scheme names, for a key other than Ed25519.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha256,
    Sha384,
}

/// A minimal DER reader: a TLV's tag, and its contents.
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return None;
        }
        let (bytes, rest) = rest.split_at_checked(n)?;
        if bytes.first() == Some(&0) {
            return None;
        }
        let len = bytes.iter().fold(0usize, |a, &x| (a << 8) | usize::from(x));
        if len < 0x80 {
            return None;
        }
        (len, rest)
    };
    let (contents, rest) = rest.split_at_checked(len)?;
    Some((tag, contents, rest))
}

const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
const OID_P224: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x21];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
const OID_P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];

fn x509(msg: &str) -> Error {
    Error::Key(format!("x509: {msg}"))
}

/// An INTEGER's magnitude, positive and minimal as Go's asn1 requires.
fn integer(b: &[u8]) -> Result<(Vec<u8>, &[u8]), Error> {
    let (tag, v, rest) = tlv(b).ok_or_else(|| x509("invalid RSA public key"))?;
    if tag != 0x02 || v.is_empty() || v.first().is_some_and(|x| x & 0x80 != 0) {
        return Err(x509("invalid RSA public key"));
    }
    if v.len() > 1 && v.first() == Some(&0) && v.get(1).is_some_and(|x| x & 0x80 == 0) {
        return Err(x509("invalid RSA public key"));
    }
    let i = v.iter().position(|x| *x != 0).unwrap_or(v.len());
    Ok((v.get(i..).unwrap_or_default().to_vec(), rest))
}

/// x509.ParsePKIXPublicKey's reading of the keys go-tuf takes.
fn parse_pkix(der: &[u8]) -> Result<PublicKey, Error> {
    let (tag, spki, rest) = tlv(der).ok_or_else(|| x509("malformed public key"))?;
    if tag != 0x30 {
        return Err(x509("malformed public key"));
    }
    if !rest.is_empty() {
        return Err(x509("trailing data after ASN.1 of public-key"));
    }
    let (tag, alg, after) = tlv(spki).ok_or_else(|| x509("malformed public key"))?;
    if tag != 0x30 {
        return Err(x509("malformed public key"));
    }
    let (tag, bits, _) = tlv(after).ok_or_else(|| x509("malformed public key"))?;
    if tag != 0x03 {
        return Err(x509("malformed public key"));
    }
    let (&unused, key) = bits.split_first().ok_or_else(|| x509("malformed public key"))?;
    if unused != 0 {
        return Err(x509("malformed public key"));
    }
    let (tag, oid, params) = tlv(alg).ok_or_else(|| x509("malformed public key"))?;
    if tag != 0x06 {
        return Err(x509("malformed public key"));
    }
    match oid {
        OID_RSA => {
            if params != [0x05, 0x00] {
                return Err(x509("RSA key missing NULL parameters"));
            }
            let (tag, seq, rest) = tlv(key).ok_or_else(|| x509("invalid RSA public key"))?;
            if tag != 0x30 || !rest.is_empty() {
                return Err(x509("trailing data after RSA public key"));
            }
            let (n, rest) = integer(seq)?;
            let (e, rest) = integer(rest)?;
            if !rest.is_empty() {
                return Err(x509("invalid RSA public key"));
            }
            if n.is_empty() {
                return Err(x509("RSA modulus is not a positive number"));
            }
            if e.is_empty() || e.len() > 4 || (e.len() == 4 && e.first().is_some_and(|x| x & 0x80 != 0)) {
                return Err(x509("RSA public exponent is not a positive number"));
            }
            Ok(PublicKey::Rsa { n, e })
        }
        OID_EC => {
            let (tag, named, _) =
                tlv(params).ok_or_else(|| x509("failed to parse ECDSA parameters as named curve"))?;
            if tag != 0x06 {
                return Err(x509("failed to parse ECDSA parameters as named curve"));
            }
            let curve = match named {
                OID_P224 => Curve::P224,
                OID_P256 => Curve::P256,
                OID_P384 => Curve::P384,
                OID_P521 => Curve::P521,
                _ => return Err(x509("unsupported elliptic curve")),
            };
            if !on_curve(curve, key) {
                return Err(x509("failed to unmarshal elliptic curve point"));
            }
            Ok(PublicKey::Ecdsa {
                curve,
                point: key.to_vec(),
            })
        }
        OID_ED25519 => {
            if !params.is_empty() {
                return Err(x509("Ed25519 key encoded with illegal parameters"));
            }
            if key.len() != 32 {
                return Err(Error::Key(format!(
                    "ed25519: bad public key length: {}",
                    key.len()
                )));
            }
            Ok(PublicKey::Ed25519(key.to_vec()))
        }
        _ => Err(x509("unknown public key algorithm")),
    }
}

/// elliptic.Unmarshal: an uncompressed point of the curve, on it.
fn on_curve(curve: Curve, point: &[u8]) -> bool {
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_FIXED, ECDSA_P384_SHA384_FIXED, ECDSA_P521_SHA512_FIXED, ParsedPublicKey,
    };
    let (size, alg) = match curve {
        Curve::P256 => (32, &ECDSA_P256_SHA256_FIXED),
        Curve::P384 => (48, &ECDSA_P384_SHA384_FIXED),
        Curve::P521 => (66, &ECDSA_P521_SHA512_FIXED),
        // AWS-LC does not check P-224 points; go-tuf keys are never P-224 in practice,
        // and one is refused as a key no verifier here can use (D104).
        Curve::P224 => return false,
    };
    point.len() == 1 + 2 * size && point.first() == Some(&4) && ParsedPublicKey::new(alg, point).is_ok()
}

/// cryptoutils.UnmarshalPEMToPublicKey.
fn from_pem(text: &str) -> Result<PublicKey, Error> {
    let block = shards_gitsign::pem::decode(text.as_bytes())
        .ok_or_else(|| Error::Key("PEM decoding failed".into()))?;
    match block.kind.as_str() {
        "PUBLIC KEY" => parse_pkix(&block.bytes),
        "RSA PUBLIC KEY" => {
            let (tag, seq, rest) = tlv(&block.bytes).ok_or_else(|| x509("invalid RSA public key"))?;
            if tag != 0x30 || !rest.is_empty() {
                return Err(x509("trailing data after RSA public key"));
            }
            let (n, rest) = integer(seq)?;
            let (e, _) = integer(rest)?;
            Ok(PublicKey::Rsa { n, e })
        }
        other => Err(Error::Key(format!(
            "unknown Public key PEM file type: {other}. Are you passing the correct public key?"
        ))),
    }
}

/// Key.ToPublicKey: the key its type names, or why not.
pub fn to_public_key(key: &Key) -> Result<PublicKey, Error> {
    match key.keytype.as_str() {
        "rsa" => match from_pem(&key.public)? {
            k @ PublicKey::Rsa { .. } => Ok(k),
            _ => Err(Error::Key("invalid rsa public key".into())),
        },
        "ecdsa" | "ecdsa-sha2-nistp256" => match from_pem(&key.public)? {
            k @ PublicKey::Ecdsa { .. } => Ok(k),
            _ => Err(Error::Key("invalid ecdsa public key".into())),
        },
        "ed25519" => {
            let b = key.public.as_bytes();
            if !b.len().is_multiple_of(2) {
                return Err(Error::Key("encoding/hex: odd length hex string".into()));
            }
            let mut out = Vec::with_capacity(b.len() / 2);
            for pair in b.chunks(2) {
                let d = |c: &u8| (*c as char).to_digit(16);
                match (pair.first().and_then(d), pair.get(1).and_then(d)) {
                    (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
                    _ => {
                        let c = pair.iter().copied().find(|c| !c.is_ascii_hexdigit()).unwrap_or(0);
                        return Err(Error::Key(format!(
                            "encoding/hex: invalid byte: U+{:04X} {:?}",
                            c, c as char
                        )));
                    }
                }
            }
            Ok(PublicKey::Ed25519(out))
        }
        _ => Err(Error::Key("unsupported public key type".into())),
    }
}

impl PublicKey {
    /// What identifies the key itself, as go-tuf's fingerprint of its PKIX encoding does:
    /// keys that are the same count once toward a threshold.
    pub fn fingerprint(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            PublicKey::Ecdsa { curve, point } => {
                out.push(match curve {
                    Curve::P224 => 1,
                    Curve::P256 => 2,
                    Curve::P384 => 3,
                    Curve::P521 => 4,
                });
                out.extend_from_slice(point);
            }
            PublicKey::Rsa { n, e } => {
                out.push(5);
                out.extend_from_slice(&(n.len() as u64).to_be_bytes());
                out.extend_from_slice(n);
                out.extend_from_slice(e);
            }
            PublicKey::Ed25519(k) => {
                out.push(6);
                out.extend_from_slice(k);
            }
        }
        digest::digest(&digest::SHA256, &out).as_ref().to_vec()
    }

    /// The signature verifier's check of `sig` over `payload`, with `hash` for a key other
    /// than Ed25519.
    pub fn verify(&self, hash: Hash, payload: &[u8], sig: &[u8]) -> bool {
        let h = match hash {
            Hash::Sha256 => &digest::SHA256,
            Hash::Sha384 => &digest::SHA384,
        };
        match self {
            PublicKey::Ed25519(k) => {
                use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
                k.len() == 32 && UnparsedPublicKey::new(&ED25519, k).verify(payload, sig).is_ok()
            }
            PublicKey::Ecdsa { curve, point } => {
                let d = digest::digest(h, payload);
                ecdsa_asn1(*curve, point, d.as_ref(), sig)
            }
            PublicKey::Rsa { n, e } => {
                let d = digest::digest(h, payload);
                shards_gitsign::arith::rsa_pss_verify(
                    n,
                    e,
                    match hash {
                        Hash::Sha256 => shards_gitsign::signature::Hash::Sha256,
                        Hash::Sha384 => shards_gitsign::signature::Hash::Sha384,
                    },
                    d.as_ref(),
                    sig,
                    None,
                )
            }
        }
    }
}

/// ecdsa.VerifyASN1 of a digest of any length on a curve AWS-LC knows: the digest cut to
/// the order's octets and padded to the length of the curve's own algorithm, which keeps
/// the integer ECDSA reads of it (hashToInt).
fn ecdsa_asn1(curve: Curve, point: &[u8], hashed: &[u8], sig: &[u8]) -> bool {
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_ASN1, ECDSA_P384_SHA384_ASN1, ECDSA_P521_SHA512_ASN1, ParsedPublicKey,
        VerificationAlgorithm,
    };
    let (alg, order, len, dalg): (
        &'static dyn VerificationAlgorithm,
        usize,
        usize,
        &'static digest::Algorithm,
    ) = match curve {
        Curve::P256 => (&ECDSA_P256_SHA256_ASN1, 32, 32, &digest::SHA256),
        Curve::P384 => (&ECDSA_P384_SHA384_ASN1, 48, 48, &digest::SHA384),
        Curve::P521 => (&ECDSA_P521_SHA512_ASN1, 66, 64, &digest::SHA512),
        Curve::P224 => return false,
    };
    let cut = hashed.get(..order.min(hashed.len())).unwrap_or_default();
    let Some(pad) = len.checked_sub(cut.len()) else {
        return false;
    };
    let mut e = vec![0u8; pad];
    e.extend_from_slice(cut);
    let Ok(d) = digest::Digest::import_less_safe(&e, dalg) else {
        return false;
    };
    ParsedPublicKey::new(alg, point).is_ok_and(|k| k.verify_digest_sig(&d, sig).is_ok())
}
