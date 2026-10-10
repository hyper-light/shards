//! X.509 certificates as Go 1.26's crypto/x509 parses them (parser.go) and verifies them
//! (verify.go): `parseCertificate`'s DER, every extension `processExtensions` reads with
//! its errors, chains built as `buildChains` builds them (candidates ordered by key ID,
//! issuers checked by `CheckSignatureFrom`, each valid at the time asked, the first
//! failure kept as the hint), and the extended key usages a chain allows
//! (`checkChainForKeyUsage`), its policies (`policiesValid`) and its name constraints
//! (`checkChainConstraints`, in x509_constraints). Held to Go by `tests/x509.rs` against
//! `scripts/sigstore/generate-x509`. Signatures are checked through AWS-LC, and RSA,
//! DSA's refusal and P-224 as Go does over public values.

use num_bigint::BigUint;

use crate::asn1::{self, Fields, Kind, Params, Value};
use crate::der::{self, Der, oid_text};
use crate::time::{Time, days_from_civil, days_in};

/// x509 errors, in Go's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X509Error(pub String);

impl std::fmt::Display for X509Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(s: &str) -> X509Error {
    X509Error(s.to_string())
}

/// A signature algorithm (x509.SignatureAlgorithm) by its details' row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigAlg {
    Unknown,
    Md5Rsa,
    Sha1Rsa,
    Sha256Rsa,
    Sha384Rsa,
    Sha512Rsa,
    Sha256RsaPss,
    Sha384RsaPss,
    Sha512RsaPss,
    DsaSha1,
    DsaSha256,
    EcdsaSha1,
    EcdsaSha256,
    EcdsaSha384,
    EcdsaSha512,
    Ed25519,
}

/// A hash (crypto.Hash) Go's checks take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    /// The digest of `data`; None for MD5, which Go refuses before it hashes.
    pub fn of(self, data: &[u8]) -> Vec<u8> {
        use aws_lc_rs::digest;
        let alg = match self {
            Hash::Md5 => return Vec::new(),
            Hash::Sha1 => &digest::SHA1_FOR_LEGACY_USE_ONLY,
            Hash::Sha224 => &digest::SHA224,
            Hash::Sha256 => &digest::SHA256,
            Hash::Sha384 => &digest::SHA384,
            Hash::Sha512 => &digest::SHA512,
        };
        digest::digest(alg, data).as_ref().to_vec()
    }

    pub fn size(self) -> usize {
        match self {
            Hash::Md5 => 16,
            Hash::Sha1 => 20,
            Hash::Sha224 => 28,
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
            Hash::Sha512 => 64,
        }
    }

    /// crypto.Hash.String.
    pub fn name(self) -> &'static str {
        match self {
            Hash::Md5 => "MD5",
            Hash::Sha1 => "SHA-1",
            Hash::Sha224 => "SHA-224",
            Hash::Sha256 => "SHA-256",
            Hash::Sha384 => "SHA-384",
            Hash::Sha512 => "SHA-512",
        }
    }

    pub fn gitsign(self) -> Option<shards_gitsign::signature::Hash> {
        use shards_gitsign::signature::Hash as G;
        Some(match self {
            Hash::Sha1 => G::Sha1,
            Hash::Sha224 => G::Sha224,
            Hash::Sha256 => G::Sha256,
            Hash::Sha384 => G::Sha384,
            Hash::Sha512 => G::Sha512,
            Hash::Md5 => return None,
        })
    }
}

/// The public key algorithm a signature algorithm's details name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAlg {
    None,
    Rsa,
    Dsa,
    Ecdsa,
    Ed25519,
}

impl KeyAlg {
    fn name(self) -> &'static str {
        match self {
            KeyAlg::Rsa => "RSA",
            KeyAlg::Dsa => "DSA",
            KeyAlg::Ecdsa => "ECDSA",
            KeyAlg::Ed25519 => "Ed25519",
            KeyAlg::None => "0",
        }
    }
}

impl SigAlg {
    /// signatureAlgorithmDetails: (name, key algorithm, hash; None for Ed25519's none).
    fn details(self) -> Option<(&'static str, KeyAlg, Option<Hash>)> {
        Some(match self {
            SigAlg::Unknown => return None,
            SigAlg::Md5Rsa => ("MD5-RSA", KeyAlg::Rsa, Some(Hash::Md5)),
            SigAlg::Sha1Rsa => ("SHA1-RSA", KeyAlg::Rsa, Some(Hash::Sha1)),
            SigAlg::Sha256Rsa => ("SHA256-RSA", KeyAlg::Rsa, Some(Hash::Sha256)),
            SigAlg::Sha384Rsa => ("SHA384-RSA", KeyAlg::Rsa, Some(Hash::Sha384)),
            SigAlg::Sha512Rsa => ("SHA512-RSA", KeyAlg::Rsa, Some(Hash::Sha512)),
            SigAlg::Sha256RsaPss => ("SHA256-RSAPSS", KeyAlg::Rsa, Some(Hash::Sha256)),
            SigAlg::Sha384RsaPss => ("SHA384-RSAPSS", KeyAlg::Rsa, Some(Hash::Sha384)),
            SigAlg::Sha512RsaPss => ("SHA512-RSAPSS", KeyAlg::Rsa, Some(Hash::Sha512)),
            SigAlg::DsaSha1 => ("DSA-SHA1", KeyAlg::Dsa, Some(Hash::Sha1)),
            SigAlg::DsaSha256 => ("DSA-SHA256", KeyAlg::Dsa, Some(Hash::Sha256)),
            SigAlg::EcdsaSha1 => ("ECDSA-SHA1", KeyAlg::Ecdsa, Some(Hash::Sha1)),
            SigAlg::EcdsaSha256 => ("ECDSA-SHA256", KeyAlg::Ecdsa, Some(Hash::Sha256)),
            SigAlg::EcdsaSha384 => ("ECDSA-SHA384", KeyAlg::Ecdsa, Some(Hash::Sha384)),
            SigAlg::EcdsaSha512 => ("ECDSA-SHA512", KeyAlg::Ecdsa, Some(Hash::Sha512)),
            SigAlg::Ed25519 => ("Ed25519", KeyAlg::Ed25519, None),
        })
    }

    /// SignatureAlgorithm.String.
    pub fn name(self) -> &'static str {
        self.details().map_or("0", |d| d.0)
    }

    fn is_pss(self) -> bool {
        matches!(
            self,
            SigAlg::Sha256RsaPss | SigAlg::Sha384RsaPss | SigAlg::Sha512RsaPss
        )
    }
}

pub const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
pub const OID_DSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x01];
pub const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
pub const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
pub const OID_P224: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x21];
pub const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
pub const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
pub const OID_P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];
const OID_RSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
const OID_MGF1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const OID_SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
/// id-ce-subjectAltName.
pub const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];
/// id-ce-extKeyUsage.
pub const OID_EKU: &[u8] = &[0x55, 0x1d, 0x25];
const OID_AIA: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x01];

/// signatureAlgorithmDetails' OIDs.
fn sig_alg_of(oid: &[u8]) -> SigAlg {
    const RSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01];
    const ECDSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04];
    if let Some(rest) = oid.strip_prefix(RSA_PREFIX) {
        return match rest {
            [0x04] => SigAlg::Md5Rsa,
            [0x05] => SigAlg::Sha1Rsa,
            [0x0b] => SigAlg::Sha256Rsa,
            [0x0c] => SigAlg::Sha384Rsa,
            [0x0d] => SigAlg::Sha512Rsa,
            _ => SigAlg::Unknown,
        };
    }
    if let Some(rest) = oid.strip_prefix(ECDSA_PREFIX) {
        return match rest {
            [0x01] => SigAlg::EcdsaSha1,
            [0x03, 0x02] => SigAlg::EcdsaSha256,
            [0x03, 0x03] => SigAlg::EcdsaSha384,
            [0x03, 0x04] => SigAlg::EcdsaSha512,
            _ => SigAlg::Unknown,
        };
    }
    match oid {
        [0x2b, 0x0e, 0x03, 0x02, 0x1d] => SigAlg::Sha1Rsa,
        [0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x03] => SigAlg::DsaSha1,
        [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x03, 0x02] => SigAlg::DsaSha256,
        OID_ED25519 => SigAlg::Ed25519,
        _ => SigAlg::Unknown,
    }
}

/// An AlgorithmIdentifier as parseAI reads it: its OID and its parameters' element,
/// whole (empty where there are none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ai<'a> {
    pub oid: &'a [u8],
    pub params: &'a [u8],
}

/// parseAI.
pub fn parse_ai(mut d: Der<'_>) -> Result<Ai<'_>, X509Error> {
    let oid = d.oid().ok_or_else(|| err("x509: malformed OID"))?;
    if d.is_empty() {
        return Ok(Ai { oid, params: &[] });
    }
    let (_, whole, _) = d.any_element().ok_or_else(|| err("x509: malformed parameters"))?;
    Ok(Ai { oid, params: whole })
}

/// getSignatureAlgorithmFromAI.
fn signature_algorithm(ai: &Ai<'_>) -> SigAlg {
    if ai.oid == OID_ED25519 && !ai.params.is_empty() {
        return SigAlg::Unknown;
    }
    if ai.oid != OID_RSA_PSS {
        return sig_alg_of(ai.oid);
    }
    pss_algorithm(ai.params).unwrap_or(SigAlg::Unknown)
}

/// An AlgorithmIdentifier as encoding/asn1 reads pkix.AlgorithmIdentifier: its OID's
/// arcs and its optional parameters' element whole.
fn asn1_ai(inner: &[u8]) -> Result<(Vec<u32>, &[u8]), asn1::Asn1Error> {
    let mut f = Fields::new(inner);
    let Some(Value::Oid(oid)) = f.next(Kind::Oid, &Params::default())? else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    let params = match f.next(Kind::Raw, &Params::default().optional())? {
        Some(Value::Raw { full, .. }) => full,
        _ => &[],
    };
    Ok((oid, params))
}

/// RSASSA-PSS parameters as getSignatureAlgorithmFromAI reads them with encoding/asn1
/// (pssParameters): hash [0], MGF [1] and salt [2] required, trailer [3] 1 by default;
/// MGF1 of the same hash, NULL or no parameters, a salt the hash's length.
fn pss_algorithm(params: &[u8]) -> Option<SigAlg> {
    let (v, _) = asn1::unmarshal(params, Kind::Struct, &Params::default()).ok()?;
    let Value::Struct { inner, .. } = v else {
        return None;
    };
    let mut f = Fields::new(inner);
    let Some(Value::Struct { inner: hash, .. }) = f.next(Kind::Struct, &Params::explicit(0)).ok()? else {
        return None;
    };
    let Some(Value::Struct { inner: mgf, .. }) = f.next(Kind::Struct, &Params::explicit(1)).ok()? else {
        return None;
    };
    let Some(Value::Int(salt)) = f.next(Kind::Int64, &Params::explicit(2)).ok()? else {
        return None;
    };
    let trailer = match f
        .next(Kind::Int64, &Params::explicit(3).optional().with_default(1))
        .ok()?
    {
        Some(Value::Int(t)) => t,
        _ => 1,
    };
    let (hash_oid, hash_params) = asn1_ai(hash).ok()?;
    let (mgf_oid, mgf_params) = asn1_ai(mgf).ok()?;
    let (v, _) = asn1::unmarshal(mgf_params, Kind::Struct, &Params::default()).ok()?;
    let Value::Struct { inner: mgf_hash, .. } = v else {
        return None;
    };
    let (mgf_hash_oid, mgf_hash_params) = asn1_ai(mgf_hash).ok()?;
    let null_or_none = |p: &[u8]| p.is_empty() || p == [0x05, 0x00];
    let arcs = |b: &[u8]| der::oid_arcs(b).unwrap_or_default();
    if !null_or_none(hash_params)
        || mgf_oid != arcs(OID_MGF1)
        || mgf_hash_oid != hash_oid
        || !null_or_none(mgf_hash_params)
        || trailer != 1
    {
        return None;
    }
    match salt {
        32 if hash_oid == arcs(OID_SHA256) => Some(SigAlg::Sha256RsaPss),
        48 if hash_oid == arcs(OID_SHA384) => Some(SigAlg::Sha384RsaPss),
        64 if hash_oid == arcs(OID_SHA512) => Some(SigAlg::Sha512RsaPss),
        _ => None,
    }
}

/// A public key as Go parses it (parsePublicKey); Unknown for an algorithm it leaves
/// unparsed (X25519 among them), which no signature is checked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    Unknown,
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Dsa,
    Ecdsa { curve: Curve, point: Vec<u8> },
    Ed25519(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    P224,
    P256,
    P384,
    P521,
}

impl Curve {
    /// (name, p, b, octets), as Go 1.26's crypto/elliptic carries them.
    fn params(self) -> (&'static str, &'static str, &'static str, usize) {
        match self {
            Curve::P224 => (
                "P224",
                "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000000000000000000001",
                "B4050A850C04B3ABF54132565044B0B7D7BFD8BA270B39432355FFB4",
                28,
            ),
            Curve::P256 => (
                "P256",
                "FFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF",
                "5AC635D8AA3A93E7B3EBBD55769886BC651D06B0CC53B0F63BCE3C3E27D2604B",
                32,
            ),
            Curve::P384 => (
                "P384",
                "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFFFF0000000000000000FFFFFFFF",
                "B3312FA7E23EE7E4988E056BE3F82D19181D9C6EFE8141120314088F5013875AC656398D8A2ED19D2A85C8EDD3EC2AEF",
                48,
            ),
            Curve::P521 => (
                "P521",
                "1FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
                "51953EB9618E1C9A1F929A21A0B68540EEA2DA725B99B315F3B8B489918EF109E156193951EC7E937B1652C0BD3BB1BF073573DF883D2C34F1EF451FD46B503F00",
                66,
            ),
        }
    }

    /// The curve's name as crypto/elliptic names it (`P-256`).
    pub fn go_name(self) -> &'static str {
        match self {
            Curve::P224 => "P-224",
            Curve::P256 => "P-256",
            Curve::P384 => "P-384",
            Curve::P521 => "P-521",
        }
    }

    /// The order's octets.
    pub fn order_bytes(self) -> usize {
        self.params().3
    }
}

fn big(hex: &str) -> BigUint {
    BigUint::parse_bytes(hex.as_bytes(), 16).unwrap_or_default()
}

/// ecdsa.ParseUncompressedPublicKey: an uncompressed point of the curve's length, its
/// coordinates below p, on the curve; nistec's words for each failure (P-256's from its
/// assembly, as amd64 and arm64 build it).
pub fn parse_point(curve: Curve, data: &[u8]) -> Result<(), X509Error> {
    if data.first() != Some(&4) {
        return Err(err("ecdsa: invalid uncompressed public key"));
    }
    let (name, p, b, size) = curve.params();
    if data.len() != 1 + 2 * size {
        return Err(X509Error(format!("invalid {name} point encoding")));
    }
    let p = big(p);
    let x = BigUint::from_bytes_be(data.get(1..1 + size).unwrap_or_default());
    let y = BigUint::from_bytes_be(data.get(1 + size..).unwrap_or_default());
    if x >= p || y >= p {
        return Err(X509Error(if curve == Curve::P256 {
            "invalid P256 element encoding".into()
        } else {
            format!("invalid {name}Element encoding")
        }));
    }
    // y² = x³ - 3x + b.
    let three = BigUint::from(3u8);
    let rhs = ((&x * &x * &x) + (&p * &three) - (&three * &x) % &p + big(b)) % &p;
    if (&y * &y) % &p != rhs {
        return Err(X509Error(format!("{name} point not on curve")));
    }
    Ok(())
}

/// parsePublicKey, for the algorithms getPublicKeyAlgorithmFromOID knows; Unknown for
/// the rest, unparsed.
pub fn parse_public_key(ai: &Ai<'_>, data: &[u8]) -> Result<PublicKey, X509Error> {
    match ai.oid {
        OID_RSA => {
            if ai.params != [0x05, 0x00] {
                return Err(err("x509: RSA key missing NULL parameters"));
            }
            let mut k = Der(data);
            let mut seq = k
                .read(der::SEQUENCE)
                .ok_or_else(|| err("x509: invalid RSA public key"))?;
            let n = seq
                .integer_raw()
                .ok_or_else(|| err("x509: invalid RSA modulus"))?;
            let e = seq
                .int64()
                .ok_or_else(|| err("x509: invalid RSA public exponent"))?;
            if n.first().is_some_and(|b| b & 0x80 != 0) || n.iter().all(|b| *b == 0) {
                return Err(err("x509: RSA modulus is not a positive number"));
            }
            if e <= 0 {
                return Err(err("x509: RSA public exponent is not a positive number"));
            }
            let i = n.iter().position(|x| *x != 0).unwrap_or(0);
            Ok(PublicKey::Rsa {
                n: n.get(i..).unwrap_or_default().to_vec(),
                e: e.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect(),
            })
        }
        OID_EC => {
            let mut p = Der(ai.params);
            let named = p.oid().ok_or_else(|| err("x509: invalid ECDSA parameters"))?;
            let curve = match named {
                OID_P224 => Curve::P224,
                OID_P256 => Curve::P256,
                OID_P384 => Curve::P384,
                OID_P521 => Curve::P521,
                _ => return Err(err("x509: unsupported elliptic curve")),
            };
            parse_point(curve, data)?;
            Ok(PublicKey::Ecdsa {
                curve,
                point: data.to_vec(),
            })
        }
        OID_ED25519 => {
            if !ai.params.is_empty() {
                return Err(err("x509: Ed25519 key encoded with illegal parameters"));
            }
            if data.len() != 32 {
                return Err(err("x509: wrong Ed25519 public key size"));
            }
            Ok(PublicKey::Ed25519(data.to_vec()))
        }
        OID_DSA => {
            let mut y = Der(data);
            let y = y
                .integer_raw()
                .ok_or_else(|| err("x509: invalid DSA public key"))?;
            let mut p = Der(ai.params);
            let mut seq = p
                .read(der::SEQUENCE)
                .ok_or_else(|| err("x509: invalid DSA parameters"))?;
            let mut ints = Vec::new();
            for _ in 0..3 {
                ints.push(
                    seq.integer_raw()
                        .ok_or_else(|| err("x509: invalid DSA parameters"))?,
                );
            }
            ints.push(y);
            let positive = |b: &[u8]| b.first().is_some_and(|x| x & 0x80 == 0) && b.iter().any(|x| *x != 0);
            if !ints.iter().all(|b| positive(b)) {
                return Err(err("x509: zero or negative DSA parameter"));
            }
            Ok(PublicKey::Dsa)
        }
        _ => Ok(PublicKey::Unknown),
    }
}

/// A SubjectPublicKeyInfo's algorithm OID octets, parameters and key bits.
type Spki = (Vec<u8>, Vec<u8>, Vec<u8>);

/// The fields of a SubjectPublicKeyInfo as encoding/asn1 reads publicKeyInfo: the
/// algorithm's OID octets and parameters (whole), and the key's bits right-aligned.
fn asn1_spki(inner: &[u8]) -> Result<Spki, asn1::Asn1Error> {
    let mut f = Fields::new(inner);
    let Some(Value::Struct { inner: ai, .. }) = f.next(Kind::Struct, &Params::default())? else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    let Some(Value::BitString { bytes, bit_length }) = f.next(Kind::BitString, &Params::default())? else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    let mut a = Fields::new(ai);
    // The OID as encoding/asn1 parses it, kept as its octets for the comparisons below.
    let (oid_tl, oid_at) = asn1::tag_and_length(ai, 0)?;
    let Some(Value::Oid(_)) = a.next(Kind::Oid, &Params::default())? else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    let oid = ai
        .get(oid_at..oid_at + oid_tl.length)
        .unwrap_or_default()
        .to_vec();
    let params = match a.next(Kind::Raw, &Params::default().optional())? {
        Some(Value::Raw { full, .. }) => full.to_vec(),
        _ => Vec::new(),
    };
    Ok((oid, params, asn1::right_align(bytes, bit_length)))
}

/// x509.ParsePKIXPublicKey.
pub fn parse_pkix_public_key(raw: &[u8]) -> Result<PublicKey, X509Error> {
    let parsed = asn1::unmarshal(raw, Kind::Struct, &Params::default()).and_then(|(v, rest)| match v {
        Value::Struct { inner, .. } => asn1_spki(inner).map(|f| (f, rest)),
        _ => Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into())),
    });
    let ((oid, params, data), rest) = match parsed {
        Ok(p) => p,
        Err(e) => {
            if pkcs1_fields(raw).is_ok() {
                return Err(err(
                    "x509: failed to parse public key (use ParsePKCS1PublicKey instead for this key format)",
                ));
            }
            return Err(X509Error(e.0));
        }
    };
    if !rest.is_empty() {
        return Err(err("x509: trailing data after ASN.1 of public-key"));
    }
    let ai = Ai {
        oid: &oid,
        params: &params,
    };
    match ai.oid {
        OID_RSA | OID_DSA | OID_EC | OID_ED25519 => parse_public_key(&ai, &data),
        // X25519: a key (ecdh.X25519().NewPublicKey), but never a verifier's.
        [0x2b, 0x65, 0x6e] => {
            if !params.is_empty() {
                return Err(err("x509: X25519 key encoded with illegal parameters"));
            }
            if data.len() != 32 {
                return Err(err("crypto/ecdh: invalid public key"));
            }
            Ok(PublicKey::Unknown)
        }
        _ => Err(err("x509: unknown public key algorithm")),
    }
}

/// An RSA key's (N, E) and the octets after it.
type Pkcs1<'a> = ((Vec<u8>, i64), &'a [u8]);

/// pkcs1PublicKey as encoding/asn1 reads it: N and E, and the octets after.
fn pkcs1_fields(raw: &[u8]) -> Result<Pkcs1<'_>, asn1::Asn1Error> {
    let (v, rest) = asn1::unmarshal(raw, Kind::Struct, &Params::default())?;
    let Value::Struct { inner, .. } = v else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    let mut f = Fields::new(inner);
    let Some(Value::BigInt(n)) = f.next(Kind::BigInt, &Params::default())? else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    let Some(Value::Int(e)) = f.next(Kind::Int64, &Params::default())? else {
        return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
    };
    Ok(((n.to_vec(), e), rest))
}

/// x509.ParsePKCS1PublicKey.
pub fn parse_pkcs1_public_key(raw: &[u8]) -> Result<PublicKey, X509Error> {
    let ((n, e), rest) = match pkcs1_fields(raw) {
        Ok(v) => v,
        Err(e) => {
            let spki = asn1::unmarshal(raw, Kind::Struct, &Params::default()).and_then(|(v, _)| match v {
                Value::Struct { inner, .. } => asn1_spki(inner).map(|_| ()),
                _ => Err(asn1::Asn1Error(String::new())),
            });
            if spki.is_ok() {
                return Err(err(
                    "x509: failed to parse public key (use ParsePKIXPublicKey instead for this key format)",
                ));
            }
            return Err(X509Error(e.0));
        }
    };
    if !rest.is_empty() {
        return Err(err("asn1: syntax error: trailing data"));
    }
    if n.first().is_none_or(|b| b & 0x80 != 0) || n.iter().all(|b| *b == 0) || e <= 0 {
        return Err(err("x509: public key contains zero or negative value"));
    }
    if e > i64::from(i32::MAX) {
        return Err(err("x509: public key contains large public exponent"));
    }
    let i = n.iter().position(|x| *x != 0).unwrap_or(0);
    Ok(PublicKey::Rsa {
        n: n.get(i..).unwrap_or_default().to_vec(),
        e: e.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect(),
    })
}

/// An attribute of a name as parseName reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atv {
    pub oid: Vec<u8>,
    pub value: String,
}

/// x509's isPrintable (with `*` and `&`).
fn printable(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || (b'\''..=b')').contains(&b)
        || (b'+'..=b'/').contains(&b)
        || matches!(b, b' ' | b':' | b'=' | b'?' | b'*' | b'&')
}

/// parseASN1String.
fn asn1_string(tag: u8, value: &[u8]) -> Result<String, String> {
    match tag {
        der::T61_STRING => Ok(value.iter().map(|&b| char::from(b)).collect()),
        der::PRINTABLE_STRING => {
            if value.iter().all(|b| printable(*b)) {
                Ok(String::from_utf8_lossy(value).into_owned())
            } else {
                Err("invalid PrintableString".into())
            }
        }
        der::UTF8_STRING => String::from_utf8(value.to_vec()).map_err(|_| "invalid UTF-8 string".to_string()),
        der::BMP_STRING => asn1::bmp(value).map_err(str::to_string),
        der::IA5_STRING => {
            if value.is_ascii() {
                Ok(String::from_utf8_lossy(value).into_owned())
            } else {
                Err("invalid IA5String".into())
            }
        }
        0x12 => {
            if value.iter().all(|b| b.is_ascii_digit() || *b == b' ') {
                Ok(String::from_utf8_lossy(value).into_owned())
            } else {
                Err("invalid NumericString".into())
            }
        }
        t => Err(format!("unsupported string type: {t}")),
    }
}

/// parseName: an RDNSequence's attributes, each RDN's in order.
pub fn parse_name(raw: &[u8]) -> Result<Vec<Vec<Atv>>, X509Error> {
    let mut d = Der(raw);
    let mut seq = d
        .read(der::SEQUENCE)
        .ok_or_else(|| err("x509: invalid RDNSequence"))?;
    let mut out = Vec::new();
    while !seq.is_empty() {
        let mut set = seq
            .read(der::SET)
            .ok_or_else(|| err("x509: invalid RDNSequence"))?;
        let mut rdn = Vec::new();
        while !set.is_empty() {
            let mut atv = set
                .read(der::SEQUENCE)
                .ok_or_else(|| err("x509: invalid RDNSequence: invalid attribute"))?;
            let oid = atv
                .oid()
                .ok_or_else(|| err("x509: invalid RDNSequence: invalid attribute type"))?;
            let (tag, _, value) = atv
                .any_element()
                .ok_or_else(|| err("x509: invalid RDNSequence: invalid attribute value"))?;
            let value = asn1_string(tag, value)
                .map_err(|e| X509Error(format!("x509: invalid RDNSequence: invalid attribute value: {e}")))?;
            rdn.push(Atv {
                oid: oid.to_vec(),
                value,
            });
        }
        out.push(rdn);
    }
    Ok(out)
}

/// The name's attribute of a standard type (2.5.4.n), FillFromRDNSequence's way: the last
/// value of CN and SERIALNUMBER, every value of the others.
fn std_type(oid: &[u8]) -> Option<u8> {
    match oid {
        [0x55, 0x04, n] if matches!(n, 3 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 17) => Some(*n),
        _ => None,
    }
}

/// pkix.Name.CommonName.
pub fn common_name(rdns: &[Vec<Atv>]) -> String {
    rdns.iter()
        .flatten()
        .rfind(|a| std_type(&a.oid) == Some(3))
        .map(|a| a.value.clone())
        .unwrap_or_default()
}

/// pkix.Name.Organization.
pub fn organization(rdns: &[Vec<Atv>]) -> Vec<String> {
    rdns.iter()
        .flatten()
        .filter(|a| std_type(&a.oid) == Some(10))
        .map(|a| a.value.clone())
        .collect()
}

/// An attribute as RDNSequence.String prints it: its standard type's name, or its OID.
type NameAtv = (Option<&'static str>, Vec<u8>, String);

/// pkix.Name.String of a parsed name: the attributes outside the standard set first,
/// each an RDN, then ToRDNSequence's, the whole reversed and escaped as RDNSequence.String
/// escapes it.
pub fn name_string(rdns: &[Vec<Atv>]) -> String {
    const ORDER: [(u8, &str); 9] = [
        (6, "C"),
        (8, "ST"),
        (7, "L"),
        (9, "STREET"),
        (17, "POSTALCODE"),
        (10, "O"),
        (11, "OU"),
        (3, "CN"),
        (5, "SERIALNUMBER"),
    ];
    let mut seq: Vec<Vec<NameAtv>> = Vec::new();
    for atv in rdns.iter().flatten() {
        if std_type(&atv.oid).is_none() {
            seq.push(vec![(None, atv.oid.clone(), atv.value.clone())]);
        }
    }
    for (n, name) in ORDER {
        let values: Vec<&str> = rdns
            .iter()
            .flatten()
            .filter(|a| std_type(&a.oid) == Some(n))
            .map(|a| a.value.as_str())
            .collect();
        let values: Vec<&str> = match n {
            3 | 5 => values
                .last()
                .filter(|v| !v.is_empty())
                .map(|v| vec![*v])
                .unwrap_or_default(),
            _ => values,
        };
        if !values.is_empty() {
            seq.push(
                values
                    .iter()
                    .map(|v| (Some(name), Vec::new(), (*v).to_string()))
                    .collect(),
            );
        }
    }
    let mut out = String::new();
    for (i, rdn) in seq.iter().rev().enumerate() {
        if i > 0 {
            out.push(',');
        }
        for (j, (name, oid, value)) in rdn.iter().enumerate() {
            if j > 0 {
                out.push('+');
            }
            match name {
                Some(n) => {
                    out.push_str(n);
                    out.push('=');
                    let last = value.len().saturating_sub(1);
                    for (k, c) in value.char_indices() {
                        let escape = match c {
                            ',' | '+' | '"' | '\\' | '<' | '>' | ';' => true,
                            ' ' => k == 0 || k == last,
                            '#' => k == 0,
                            _ => false,
                        };
                        if escape {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                }
                None => {
                    // asn1.Marshal of the string: PrintableString where every character
                    // is printable, else UTF8String.
                    let strict = |b: u8| {
                        b.is_ascii_alphanumeric()
                            || (b'\''..=b')').contains(&b)
                            || (b'+'..=b'/').contains(&b)
                            || matches!(b, b' ' | b':' | b'=' | b'?')
                    };
                    let tag = if value.bytes().all(strict) {
                        der::PRINTABLE_STRING
                    } else {
                        der::UTF8_STRING
                    };
                    let derb = der::tlv(tag, value.as_bytes());
                    out.push_str(&oid_text(oid));
                    out.push_str("=#");
                    out.extend(derb.iter().map(|b| format!("{b:02x}")));
                }
            }
        }
    }
    out
}

/// UTCTime or GeneralizedTime as cryptobyte reads them for parseTime.
fn parse_time(d: &mut Der<'_>) -> Result<Time, X509Error> {
    if d.peek(der::UTC_TIME) {
        let b = d
            .read(der::UTC_TIME)
            .ok_or_else(|| err("x509: malformed UTCTime"))?
            .0;
        // The seconds layout first, then the minute one; each must print back as given.
        let t = ["060102150405Z0700", "0601021504Z0700"]
            .iter()
            .find_map(|l| crate::gotime::parse(l, b).ok().map(|t| (l, t)))
            .filter(|(l, t)| crate::gotime::format(l, t).as_bytes() == b)
            .map(|(_, t)| t)
            .ok_or_else(|| err("x509: malformed UTCTime"))?;
        Ok(if crate::gotime::year(&t) >= 2050 {
            crate::gotime::add_years(&t, -100)
        } else {
            t
        })
    } else if d.peek(der::GENERALIZED_TIME) {
        let b = d
            .read(der::GENERALIZED_TIME)
            .ok_or_else(|| err("x509: malformed GeneralizedTime"))?
            .0;
        crate::gotime::parse_exact("20060102150405Z0700", b)
            .map_err(|_| err("x509: malformed GeneralizedTime"))
    } else {
        Err(err("x509: unsupported time format"))
    }
}

/// A certificate's extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub oid: Vec<u8>,
    pub critical: bool,
    pub value: Vec<u8>,
}

/// Extended key usages Go knows (ExtKeyUsage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eku {
    Any,
    ServerAuth,
    ClientAuth,
    CodeSigning,
    EmailProtection,
    IpsecEndSystem,
    IpsecTunnel,
    IpsecUser,
    TimeStamping,
    OcspSigning,
    MicrosoftServerGatedCrypto,
    NetscapeServerGatedCrypto,
    MicrosoftCommercialCodeSigning,
    MicrosoftKernelCodeSigning,
}

fn eku_of(oid: &[u8]) -> Option<Eku> {
    const KP: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03];
    if oid == [0x55, 0x1d, 0x25, 0x00] {
        return Some(Eku::Any);
    }
    if let Some(rest) = oid.strip_prefix(KP) {
        return match rest {
            [1] => Some(Eku::ServerAuth),
            [2] => Some(Eku::ClientAuth),
            [3] => Some(Eku::CodeSigning),
            [4] => Some(Eku::EmailProtection),
            [5] => Some(Eku::IpsecEndSystem),
            [6] => Some(Eku::IpsecTunnel),
            [7] => Some(Eku::IpsecUser),
            [8] => Some(Eku::TimeStamping),
            [9] => Some(Eku::OcspSigning),
            _ => None,
        };
    }
    match oid {
        [0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x0a, 0x03, 0x03] => Some(Eku::MicrosoftServerGatedCrypto),
        [0x60, 0x86, 0x48, 0x01, 0x86, 0xf8, 0x42, 0x04, 0x01] => Some(Eku::NetscapeServerGatedCrypto),
        [0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x01, 0x16] => {
            Some(Eku::MicrosoftCommercialCodeSigning)
        }
        [0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x3d, 0x01, 0x01] => Some(Eku::MicrosoftKernelCodeSigning),
        _ => None,
    }
}

/// A parsed certificate: what Go keeps of it that verification and Sigstore read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub raw: Vec<u8>,
    pub raw_tbs: Vec<u8>,
    pub raw_issuer: Vec<u8>,
    pub raw_subject: Vec<u8>,
    pub raw_spki: Vec<u8>,
    pub version: i64,
    /// The serial's two's complement octets, as given.
    pub serial: Vec<u8>,
    pub sig_alg: SigAlg,
    pub signature: Vec<u8>,
    pub issuer: Vec<Vec<Atv>>,
    pub subject: Vec<Vec<Atv>>,
    pub not_before: Time,
    pub not_after: Time,
    pub public_key: PublicKey,
    pub extensions: Vec<Extension>,
    pub key_usage: u16,
    pub basic_constraints_valid: bool,
    pub is_ca: bool,
    /// -1 where none is set.
    pub max_path_len: i64,
    pub ext_key_usage: Vec<Eku>,
    pub unknown_ext_key_usage: Vec<Vec<u8>>,
    pub dns_names: Vec<String>,
    pub emails: Vec<String>,
    pub ips: Vec<Vec<u8>>,
    /// Each URI as url.URL.String prints the parsed one.
    pub uris: Vec<String>,
    /// Each URI's host as url.Parse reads it (URL.Host).
    pub uri_hosts: Vec<Vec<u8>>,
    pub subject_key_id: Vec<u8>,
    pub authority_key_id: Vec<u8>,
    pub unhandled_critical: Vec<Vec<u8>>,
    /// The name constraints extension, where there is one.
    pub name_constraints: Option<crate::x509_constraints::NameConstraints>,
    /// Certificate policies' OIDs, each as encoded.
    pub policies: Vec<Vec<u8>>,
    /// Policy mappings: (issuer domain, subject domain) OIDs as encoded.
    pub policy_mappings: Vec<(Vec<u8>, Vec<u8>)>,
    /// RequireExplicitPolicy, InhibitPolicyMapping and InhibitAnyPolicy, where given.
    pub require_explicit_policy: Option<i64>,
    pub inhibit_policy_mapping: Option<i64>,
    pub inhibit_any_policy: Option<i64>,
}

impl Certificate {
    /// x509.ParseCertificate.
    pub fn parse(der_bytes: &[u8]) -> Result<Certificate, X509Error> {
        let c = Certificate::parse_first(der_bytes)?;
        if c.raw.len() != der_bytes.len() {
            return Err(err("x509: trailing data"));
        }
        Ok(c)
    }

    /// x509.ParseCertificates: certificates one after another.
    pub fn parse_all(mut der_bytes: &[u8]) -> Result<Vec<Certificate>, X509Error> {
        let mut out = Vec::new();
        while !der_bytes.is_empty() {
            let c = Certificate::parse_first(der_bytes)?;
            der_bytes = der_bytes.get(c.raw.len()..).unwrap_or_default();
            out.push(c);
        }
        Ok(out)
    }

    /// parseCertificate: the certificate at the start of `der_bytes`.
    fn parse_first(der_bytes: &[u8]) -> Result<Certificate, X509Error> {
        let mut input = Der(der_bytes);
        let raw = input
            .read_element(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed certificate"))?;
        let mut cert = Der(raw)
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed certificate"))?;
        let raw_tbs = cert
            .read_element(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed tbs certificate"))?;
        let mut tbs = Der(raw_tbs)
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed tbs certificate"))?;
        let version = match tbs
            .optional(der::explicit(0))
            .ok_or_else(|| err("x509: malformed version"))?
        {
            None => 0,
            Some(mut v) => v
                .int64()
                .filter(|_| v.is_empty())
                .ok_or_else(|| err("x509: malformed version"))?,
        };
        if version < 0 {
            return Err(err("x509: malformed version"));
        }
        let version = version + 1;
        if version > 3 {
            return Err(err("x509: invalid version"));
        }
        let serial = tbs
            .integer_raw()
            .ok_or_else(|| err("x509: malformed serial number"))?;
        if serial.first().is_some_and(|b| b & 0x80 != 0) {
            return Err(err("x509: negative serial number"));
        }
        let sig_ai = tbs
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed signature algorithm identifier"))?;
        let outer_ai = cert
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed algorithm identifier"))?;
        if outer_ai.0 != sig_ai.0 {
            return Err(err(
                "x509: inner and outer signature algorithm identifiers don't match",
            ));
        }
        let sig_alg = signature_algorithm(&parse_ai(sig_ai)?);
        let raw_issuer = tbs
            .read_element(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed issuer"))?;
        let issuer = parse_name(raw_issuer)?;
        let mut validity = tbs
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed validity"))?;
        let not_before = parse_time(&mut validity)?;
        let not_after = parse_time(&mut validity)?;
        let raw_subject = tbs
            .read_element(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed issuer"))?;
        let subject = parse_name(raw_subject)?;
        let raw_spki = tbs
            .read_element(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed spki"))?;
        let mut spki = Der(raw_spki)
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: malformed spki"))?;
        let pk_ai = parse_ai(
            spki.read(der::SEQUENCE)
                .ok_or_else(|| err("x509: malformed public key algorithm identifier"))?,
        )?;
        let (bits, unused) = spki
            .bit_string()
            .ok_or_else(|| err("x509: malformed subjectPublicKey"))?;
        let public_key = if matches!(pk_ai.oid, OID_RSA | OID_DSA | OID_EC | OID_ED25519) {
            parse_public_key(
                &pk_ai,
                &asn1::right_align(bits, bits.len() * 8 - usize::from(unused)),
            )?
        } else {
            PublicKey::Unknown
        };
        let mut c = Certificate {
            raw: raw.to_vec(),
            raw_tbs: raw_tbs.to_vec(),
            raw_issuer: raw_issuer.to_vec(),
            raw_subject: raw_subject.to_vec(),
            raw_spki: raw_spki.to_vec(),
            version,
            serial: serial.to_vec(),
            sig_alg,
            signature: Vec::new(),
            issuer,
            subject,
            not_before,
            not_after,
            public_key,
            extensions: Vec::new(),
            key_usage: 0,
            basic_constraints_valid: false,
            is_ca: false,
            max_path_len: -1,
            ext_key_usage: Vec::new(),
            unknown_ext_key_usage: Vec::new(),
            dns_names: Vec::new(),
            emails: Vec::new(),
            ips: Vec::new(),
            uris: Vec::new(),
            uri_hosts: Vec::new(),
            subject_key_id: Vec::new(),
            authority_key_id: Vec::new(),
            unhandled_critical: Vec::new(),
            name_constraints: None,
            policies: Vec::new(),
            policy_mappings: Vec::new(),
            require_explicit_policy: None,
            inhibit_policy_mapping: None,
            inhibit_any_policy: None,
        };
        if version > 1 {
            tbs.skip_optional(der::implicit(1))
                .ok_or_else(|| err("x509: malformed issuerUniqueID"))?;
            tbs.skip_optional(der::implicit(2))
                .ok_or_else(|| err("x509: malformed subjectUniqueID"))?;
            if version == 3
                && let Some(mut exts) = tbs
                    .optional(der::explicit(3))
                    .ok_or_else(|| err("x509: malformed extensions"))?
            {
                let mut seq = exts
                    .read(der::SEQUENCE)
                    .ok_or_else(|| err("x509: malformed extensions"))?;
                while !seq.is_empty() {
                    let mut e = seq
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: malformed extension"))?;
                    let oid = e
                        .oid()
                        .ok_or_else(|| err("x509: malformed extension OID field"))?;
                    let critical = if e.peek(der::BOOLEAN) {
                        e.boolean()
                            .ok_or_else(|| err("x509: malformed extension critical field"))?
                    } else {
                        false
                    };
                    let value = e
                        .read(der::OCTET_STRING)
                        .ok_or_else(|| err("x509: malformed extension value field"))?;
                    if c.extensions.iter().any(|x| x.oid == oid) {
                        return Err(X509Error(format!(
                            "x509: certificate contains duplicate extension with OID \"{}\"",
                            oid_text(oid)
                        )));
                    }
                    c.extensions.push(Extension {
                        oid: oid.to_vec(),
                        critical,
                        value: value.0.to_vec(),
                    });
                }
                c.process_extensions()?;
            }
        }
        let (sig, unused) = cert
            .bit_string()
            .ok_or_else(|| err("x509: malformed signature"))?;
        c.signature = asn1::right_align(sig, sig.len() * 8 - usize::from(unused));
        Ok(c)
    }

    /// processExtensions.
    fn process_extensions(&mut self) -> Result<(), X509Error> {
        let exts = self.extensions.clone();
        for e in &exts {
            let mut unhandled = false;
            match e.oid.as_slice() {
                [0x55, 0x1d, 15] => {
                    let mut d = Der(&e.value);
                    let (bits, _) = d.bit_string().ok_or_else(|| err("x509: invalid key usage"))?;
                    let mut usage = 0u16;
                    for i in 0..9 {
                        let byte = bits.get(i / 8).copied().unwrap_or(0);
                        if byte & (0x80 >> (i % 8)) != 0 {
                            usage |= 1 << i;
                        }
                    }
                    self.key_usage = usage;
                }
                [0x55, 0x1d, 19] => {
                    let mut d = Der(&e.value);
                    let mut seq = d
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid basic constraints"))?;
                    let mut is_ca = false;
                    if seq.peek(der::BOOLEAN) {
                        is_ca = seq
                            .boolean()
                            .ok_or_else(|| err("x509: invalid basic constraints"))?;
                    }
                    let mut max = -1i64;
                    if seq.peek(der::INTEGER) {
                        let v = seq
                            .uint64()
                            .filter(|v| *v <= i64::MAX as u64)
                            .ok_or_else(|| err("x509: invalid basic constraints"))?;
                        max = i64::try_from(v).unwrap_or(i64::MAX);
                    }
                    self.is_ca = is_ca;
                    self.max_path_len = max;
                    self.basic_constraints_valid = true;
                }
                [0x55, 0x1d, 17] => {
                    self.parse_san(&e.value)?;
                    if self.dns_names.is_empty()
                        && self.emails.is_empty()
                        && self.ips.is_empty()
                        && self.uris.is_empty()
                    {
                        unhandled = true;
                    }
                }
                [0x55, 0x1d, 30] => {
                    let (nc, u) = crate::x509_constraints::parse_name_constraints(e)?;
                    unhandled = u;
                    self.name_constraints = Some(nc);
                }
                [0x55, 0x1d, 31] => {
                    let mut val = Der(&e.value);
                    let mut val = val
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid CRL distribution points"))?;
                    while !val.is_empty() {
                        let mut dp = val
                            .read(der::SEQUENCE)
                            .ok_or_else(|| err("x509: invalid CRL distribution point"))?;
                        let Some(mut name) = dp
                            .optional(der::explicit(0))
                            .ok_or_else(|| err("x509: invalid CRL distribution point"))?
                        else {
                            continue;
                        };
                        let mut name = name
                            .read(der::explicit(0))
                            .ok_or_else(|| err("x509: invalid CRL distribution point"))?;
                        while !name.is_empty() {
                            if !name.peek(der::implicit(6)) {
                                break;
                            }
                            name.read(der::implicit(6))
                                .ok_or_else(|| err("x509: invalid CRL distribution point"))?;
                        }
                    }
                }
                [0x55, 0x1d, 35] => {
                    if e.critical {
                        return Err(err("x509: authority key identifier incorrectly marked critical"));
                    }
                    let mut d = Der(&e.value);
                    let mut akid = d
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid authority key identifier"))?;
                    if akid.peek(der::implicit(0)) {
                        let id = akid
                            .read(der::implicit(0))
                            .ok_or_else(|| err("x509: invalid authority key identifier"))?;
                        self.authority_key_id = id.0.to_vec();
                    }
                }
                [0x55, 0x1d, 36] => {
                    let mut val = Der(&e.value);
                    let mut val = val
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid policy constraints extension"))?;
                    if val.peek(der::implicit(0)) {
                        self.require_explicit_policy = Some(
                            val.int64_with_tag(der::implicit(0))
                                .ok_or_else(|| err("x509: invalid policy constraints extension"))?,
                        );
                    }
                    if val.peek(der::implicit(1)) {
                        self.inhibit_policy_mapping = Some(
                            val.int64_with_tag(der::implicit(1))
                                .ok_or_else(|| err("x509: invalid policy constraints extension"))?,
                        );
                    }
                }
                [0x55, 0x1d, 37] => {
                    let mut d = Der(&e.value);
                    let mut seq = d
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid extended key usages"))?;
                    while !seq.is_empty() {
                        let oid = seq
                            .oid()
                            .ok_or_else(|| err("x509: invalid extended key usages"))?;
                        match eku_of(oid) {
                            Some(u) => self.ext_key_usage.push(u),
                            None => self.unknown_ext_key_usage.push(oid.to_vec()),
                        }
                    }
                }
                [0x55, 0x1d, 14] => {
                    if e.critical {
                        return Err(err("x509: subject key identifier incorrectly marked critical"));
                    }
                    let mut d = Der(&e.value);
                    let id = d
                        .read(der::OCTET_STRING)
                        .ok_or_else(|| err("x509: invalid subject key identifier"))?;
                    self.subject_key_id = id.0.to_vec();
                }
                [0x55, 0x1d, 32] => {
                    let mut d = Der(&e.value);
                    let mut seq = d
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid certificate policies"))?;
                    let mut seen: Vec<&[u8]> = Vec::new();
                    while !seq.is_empty() {
                        let mut cp = seq
                            .read(der::SEQUENCE)
                            .ok_or_else(|| err("x509: invalid certificate policies"))?;
                        let oid = cp
                            .read(der::OID)
                            .ok_or_else(|| err("x509: invalid certificate policies"))?
                            .0;
                        if seen.contains(&oid) || !crate::x509_constraints::new_oid_from_der(oid) {
                            return Err(err("x509: invalid certificate policies"));
                        }
                        seen.push(oid);
                    }
                    self.policies = seen.iter().map(|o| o.to_vec()).collect();
                }
                [0x55, 0x1d, 33] => {
                    let mut val = Der(&e.value);
                    let mut val = val
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid policy mappings extension"))?;
                    while !val.is_empty() {
                        let mut s = val
                            .read(der::SEQUENCE)
                            .ok_or_else(|| err("x509: invalid policy mappings extension"))?;
                        let issuer = s
                            .read(der::OID)
                            .ok_or_else(|| err("x509: invalid policy mappings extension"))?;
                        let subject = s
                            .read(der::OID)
                            .ok_or_else(|| err("x509: invalid policy mappings extension"))?;
                        self.policy_mappings.push((issuer.0.to_vec(), subject.0.to_vec()));
                    }
                }
                [0x55, 0x1d, 54] => {
                    let mut val = Der(&e.value);
                    self.inhibit_any_policy = Some(
                        val.int64()
                            .ok_or_else(|| err("x509: invalid inhibit any policy extension"))?,
                    );
                }
                [0x55, 0x1d, _] => unhandled = true,
                OID_AIA => {
                    if e.critical {
                        return Err(err("x509: authority info access incorrectly marked critical"));
                    }
                    let mut val = Der(&e.value);
                    let mut val = val
                        .read(der::SEQUENCE)
                        .ok_or_else(|| err("x509: invalid authority info access"))?;
                    while !val.is_empty() {
                        let mut aia = val
                            .read(der::SEQUENCE)
                            .ok_or_else(|| err("x509: invalid authority info access"))?;
                        aia.oid()
                            .ok_or_else(|| err("x509: invalid authority info access"))?;
                        if !aia.peek(der::implicit(6)) {
                            continue;
                        }
                        aia.read(der::implicit(6))
                            .ok_or_else(|| err("x509: invalid authority info access"))?;
                    }
                }
                _ => unhandled = true,
            }
            if e.critical && unhandled {
                self.unhandled_critical.push(e.oid.clone());
            }
        }
        Ok(())
    }

    /// parseSANExtension.
    fn parse_san(&mut self, value: &[u8]) -> Result<(), X509Error> {
        let mut d = Der(value);
        let mut seq = d
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: invalid subject alternative names"))?;
        while !seq.is_empty() {
            let (tag, _, data) = seq
                .any_element()
                .ok_or_else(|| err("x509: invalid subject alternative name"))?;
            let text = |what: &str| -> Result<String, X509Error> {
                if data.is_ascii() {
                    Ok(String::from_utf8_lossy(data).into_owned())
                } else {
                    Err(X509Error(format!("x509: SAN {what} is malformed")))
                }
            };
            match tag {
                0x81 => self.emails.push(text("rfc822Name")?),
                0x82 => self.dns_names.push(text("dNSName")?),
                0x86 => {
                    let uri = text("uniformResourceIdentifier")?;
                    let parsed = shards_dockerfile::url::parse(uri.as_bytes()).map_err(|e| {
                        X509Error(format!(
                            "x509: cannot parse URI {}: {}",
                            shards_dockerfile::go::quote(uri.as_bytes()),
                            String::from_utf8_lossy(&e)
                        ))
                    })?;
                    if !parsed.host.is_empty() && !domain_name_valid(&parsed.host, false) {
                        return Err(X509Error(format!(
                            "x509: cannot parse URI {}: invalid domain",
                            shards_dockerfile::go::quote(uri.as_bytes())
                        )));
                    }
                    self.uris
                        .push(String::from_utf8_lossy(&parsed.string()).into_owned());
                    self.uri_hosts.push(parsed.host.clone());
                }
                0x87 => match data.len() {
                    4 | 16 => self.ips.push(data.to_vec()),
                    n => return Err(X509Error(format!("x509: cannot parse IP address of length {n}"))),
                },
                _ => {}
            }
        }
        Ok(())
    }

    /// The certificate's issuer as pkix.Name.String writes it.
    pub fn issuer_string(&self) -> String {
        name_string(&self.issuer)
    }

    /// An extension by its OID.
    pub fn extension(&self, oid: &[u8]) -> Option<&Extension> {
        self.extensions.iter().find(|e| e.oid == oid)
    }

    /// The serial number in decimal (big.Int.String).
    pub fn serial_string(&self) -> String {
        BigUint::from_bytes_be(&self.serial).to_string()
    }
}

/// domainNameValid.
pub fn domain_name_valid(s: &[u8], constraint: bool) -> bool {
    if s.is_empty() {
        return true;
    }
    if s.last() == Some(&b'.') {
        return false;
    }
    let s = if constraint && s.first() == Some(&b'.') {
        s.get(1..).unwrap_or_default()
    } else {
        s
    };
    let mut last_dot: Option<usize> = None;
    for i in 0..=s.len() {
        if let Some(&c) = s.get(i)
            && !(33..=126).contains(&c)
        {
            return false;
        }
        if i == s.len() || s.get(i) == Some(&b'.') {
            let label = match last_dot {
                Some(d) => i - d - 1,
                None => i,
            };
            if label == 0 {
                return false;
            }
            last_dot = Some(i);
        }
    }
    true
}

/// What crypto/rsa refuses of a public key before it verifies with it (checkPublicKeySize,
/// then fips140/rsa's checkPublicKey), in its order: `shards_gitsign`'s checks, then the
/// exponent's bound, which they lack. x509 parses an exponent up to an int64's; crypto/rsa
/// takes none past 2^31-1.
pub fn rsa_key_error(n: &[u8], e: &[u8]) -> Option<String> {
    shards_gitsign::arith::rsa_key_error(n, e).or_else(|| {
        let e = e
            .get(e.iter().position(|b| *b != 0).unwrap_or(e.len())..)
            .unwrap_or_default();
        (e.len() > 4 || (e.len() == 4 && e.first().is_some_and(|b| b & 0x80 != 0)))
            .then(|| "crypto/rsa: public exponent too large".to_string())
    })
}

/// checkSignature: `signed` by the algorithm's hash, against `sig` by `key`.
pub fn check_signature(
    alg: SigAlg,
    signed: &[u8],
    sig: &[u8],
    key: &PublicKey,
    allow_sha1: bool,
) -> Result<(), X509Error> {
    let unsupported = || err("x509: cannot verify signature: algorithm unimplemented");
    let (key_alg, hash) = match alg.details() {
        Some((_, k, h)) => (k, h),
        None => (KeyAlg::None, None),
    };
    let insecure = || {
        X509Error(format!(
            "x509: cannot verify signature: insecure algorithm {}",
            alg.name()
        ))
    };
    let digest = match hash {
        None => {
            if key_alg != KeyAlg::Ed25519 {
                return Err(unsupported());
            }
            None
        }
        Some(Hash::Md5) => return Err(insecure()),
        Some(Hash::Sha1) if !allow_sha1 => return Err(insecure()),
        Some(h) => Some((h, h.of(signed))),
    };
    let mismatch = |have: &str| {
        X509Error(format!(
            "x509: signature algorithm specifies an {} public key, but have public key of type {have}",
            key_alg.name()
        ))
    };
    match key {
        PublicKey::Rsa { n, e } => {
            if key_alg != KeyAlg::Rsa {
                return Err(mismatch("*rsa.PublicKey"));
            }
            let (h, hashed) = digest.ok_or_else(unsupported)?;
            let g = h.gitsign().ok_or_else(unsupported)?;
            if let Some(e) = rsa_key_error(n, e) {
                return Err(X509Error(e));
            }
            let ok = if alg.is_pss() {
                shards_gitsign::arith::rsa_pss_verify(n, e, g, &hashed, sig, Some(h.size()))
            } else {
                shards_gitsign::arith::rsa_pkcs1_verify(n, e, g, &hashed, sig)
            };
            ok.then_some(())
                .ok_or_else(|| err("crypto/rsa: verification error"))
        }
        PublicKey::Ecdsa { curve, point } => {
            if key_alg != KeyAlg::Ecdsa {
                return Err(mismatch("*ecdsa.PublicKey"));
            }
            let (_, hashed) = digest.ok_or_else(unsupported)?;
            ecdsa_asn1(*curve, point, &hashed, sig)
                .then_some(())
                .ok_or_else(|| err("x509: ECDSA verification failure"))
        }
        PublicKey::Ed25519(k) => {
            if key_alg != KeyAlg::Ed25519 {
                return Err(mismatch("ed25519.PublicKey"));
            }
            use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
            UnparsedPublicKey::new(&ED25519, k)
                .verify(signed, sig)
                .map_err(|_| err("x509: Ed25519 verification failure"))
        }
        PublicKey::Dsa | PublicKey::Unknown => Err(unsupported()),
    }
}

/// An ECDSA signature's (r, s) as cryptobyte reads it for VerifyASN1: a SEQUENCE of two
/// minimal positive INTEGERs and nothing after.
pub fn ecdsa_rs(sig: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut d = Der(sig);
    let mut seq = d.read(der::SEQUENCE)?;
    let r = seq.unsigned_bytes()?;
    let s = seq.unsigned_bytes()?;
    if !d.is_empty() || !seq.is_empty() {
        return None;
    }
    Some((r, s))
}

/// ecdsa.VerifyASN1 of a digest of any length: hashToInt's cut, through AWS-LC where it
/// knows the curve and Go's own arithmetic for P-224.
pub fn ecdsa_asn1(curve: Curve, point: &[u8], hashed: &[u8], sig: &[u8]) -> bool {
    let Some((r, s)) = ecdsa_rs(sig) else {
        return false;
    };
    ecdsa_verify(curve, point, hashed, r, s)
}

/// ecdsa.Verify of (r, s), each a magnitude's octets.
pub fn ecdsa_verify(curve: Curve, point: &[u8], hashed: &[u8], r: &[u8], s: &[u8]) -> bool {
    use aws_lc_rs::digest;
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_FIXED, ECDSA_P384_SHA384_FIXED, ECDSA_P521_SHA512_FIXED, ParsedPublicKey,
        VerificationAlgorithm,
    };
    let (alg, size, len, dalg): (
        &'static dyn VerificationAlgorithm,
        usize,
        usize,
        &'static digest::Algorithm,
    ) = match curve {
        Curve::P256 => (&ECDSA_P256_SHA256_FIXED, 32, 32, &digest::SHA256),
        Curve::P384 => (&ECDSA_P384_SHA384_FIXED, 48, 48, &digest::SHA384),
        Curve::P521 => (&ECDSA_P521_SHA512_FIXED, 66, 64, &digest::SHA512),
        Curve::P224 => {
            let c = shards_gitsign::arith::NIST_P224;
            let Some((x, y)) = c.point(point) else {
                return false;
            };
            return c.verify(&x, &y, hashed, r, s);
        }
    };
    // r and s in [1, n) are AWS-LC's to check; here they only must fit the curve.
    let strip = |b: &[u8]| -> Option<Vec<u8>> {
        let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
        let v = b.get(i..)?;
        if v.len() > size {
            return None;
        }
        let mut out = vec![0u8; size - v.len()];
        out.extend_from_slice(v);
        Some(out)
    };
    let (Some(r), Some(s)) = (strip(r), strip(s)) else {
        return false;
    };
    let mut fixed = r;
    fixed.extend(s);
    // hashToInt: the digest cut to the order's octets (the bits beyond the order's are
    // AWS-LC's to drop, as Go drops them), padded to the curve's own digest length.
    let cut = hashed.get(..size.min(hashed.len())).unwrap_or_default();
    if cut.len() > len {
        // P-521's order is 521 bits: a digest over 64 octets is cut to 521 bits, which
        // AWS-LC's 64-octet digest cannot hold; Go's arithmetic takes it.
        let c = shards_gitsign::arith::NIST_P521;
        let Some((x, y)) = c.point(point) else {
            return false;
        };
        let (r, s) = fixed.split_at(size);
        return c.verify(&x, &y, hashed, r, s);
    }
    let mut e = vec![0u8; len - cut.len()];
    e.extend_from_slice(cut);
    let Ok(d) = digest::Digest::import_less_safe(&e, dalg) else {
        return false;
    };
    ParsedPublicKey::new(alg, point).is_ok_and(|k| k.verify_digest_sig(&d, &fixed).is_ok())
}

/// A pool of certificates (CertPool): in the order added, each once.
#[derive(Debug, Clone, Default)]
pub struct Pool {
    pub certs: Vec<Certificate>,
}

impl Pool {
    /// AddCert: a certificate already held is not added again.
    pub fn add(&mut self, c: Certificate) {
        if !self.contains(&c) {
            self.certs.push(c);
        }
    }

    pub fn contains(&self, c: &Certificate) -> bool {
        self.certs.iter().any(|x| x.raw == c.raw)
    }

    /// findPotentialParents: the certificates whose subject is `c`'s issuer, those whose
    /// key ID equals `c`'s authority key ID first, then those where one is empty, then
    /// the rest.
    fn parents<'p>(&'p self, c: &Certificate) -> Vec<&'p Certificate> {
        let (mut matching, mut one, mut mismatch) = (Vec::new(), Vec::new(), Vec::new());
        for p in self.certs.iter().filter(|p| p.raw_subject == c.raw_issuer) {
            if p.subject_key_id == c.authority_key_id {
                matching.push(p);
            } else if p.subject_key_id.is_empty() != c.authority_key_id.is_empty() {
                one.push(p);
            } else {
                mismatch.push(p);
            }
        }
        matching.append(&mut one);
        matching.append(&mut mismatch);
        matching
    }
}

/// What verification holds a chain to (VerifyOptions).
#[derive(Debug)]
pub struct Options<'a> {
    pub roots: &'a Pool,
    pub intermediates: &'a Pool,
    pub now: Time,
    /// Empty asks for ServerAuth, as Go defaults.
    pub key_usages: Vec<Eku>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind509 {
    Leaf,
    Intermediate,
    Root,
}

impl Certificate {
    /// isValid.
    fn is_valid(&self, kind: Kind509, chain: &[&Certificate], opts: &Options<'_>) -> Result<(), X509Error> {
        if !self.unhandled_critical.is_empty() {
            return Err(err("x509: unhandled critical extension"));
        }
        if let Some(child) = chain.last()
            && child.raw_issuer != self.raw_subject
        {
            return Err(err(
                "x509: issuer name does not match subject from issuing certificate",
            ));
        }
        if opts.now < self.not_before {
            return Err(X509Error(format!(
                "x509: certificate has expired or is not yet valid: current time {} is before {}",
                opts.now.rfc3339(),
                self.not_before.rfc3339()
            )));
        }
        if opts.now > self.not_after {
            return Err(X509Error(format!(
                "x509: certificate has expired or is not yet valid: current time {} is after {}",
                opts.now.rfc3339(),
                self.not_after.rfc3339()
            )));
        }
        if kind == Kind509::Intermediate && (!self.basic_constraints_valid || !self.is_ca) {
            return Err(err(
                "x509: certificate is not authorized to sign other certificates",
            ));
        }
        if self.basic_constraints_valid && self.max_path_len >= 0 {
            let intermediates = i64::try_from(chain.len()).unwrap_or(i64::MAX) - 1;
            if intermediates > self.max_path_len {
                return Err(err("x509: too many intermediates for path length constraint"));
            }
        }
        Ok(())
    }

    /// CheckSignatureFrom.
    pub fn check_signature_from(&self, parent: &Certificate) -> Result<(), X509Error> {
        if (parent.version == 3 && !parent.basic_constraints_valid)
            || (parent.basic_constraints_valid && !parent.is_ca)
        {
            return Err(err(
                "x509: invalid signature: parent certificate cannot sign this kind of certificate",
            ));
        }
        if parent.key_usage != 0 && parent.key_usage & (1 << 5) == 0 {
            return Err(err(
                "x509: invalid signature: parent certificate cannot sign this kind of certificate",
            ));
        }
        if parent.public_key == PublicKey::Unknown {
            return Err(err("x509: cannot verify signature: algorithm unimplemented"));
        }
        check_signature(
            self.sig_alg,
            &self.raw_tbs,
            &self.signature,
            &parent.public_key,
            false,
        )
    }

    /// CheckSignature: SHA-1 allowed.
    pub fn check_signature(&self, alg: SigAlg, signed: &[u8], sig: &[u8]) -> Result<(), X509Error> {
        check_signature(alg, signed, sig, &self.public_key, true)
    }

    /// alreadyInChain.
    fn already_in(&self, chain: &[&Certificate]) -> bool {
        let san = self.extension(OID_SAN).map(|e| e.value.as_slice());
        for c in chain {
            if c.raw_subject != self.raw_subject || c.raw_spki != self.raw_spki {
                continue;
            }
            match (san, c.extension(OID_SAN).map(|e| e.value.as_slice())) {
                (None, None) => return true,
                (None, _) | (_, None) => return false,
                (Some(a), Some(b)) if a == b => return true,
                _ => {}
            }
        }
        false
    }

    /// Verify: the chains from this certificate to a root, each valid at `opts.now` and
    /// allowing the key usages asked.
    pub fn verify<'c>(&'c self, opts: &Options<'c>) -> Result<Vec<Vec<&'c Certificate>>, X509Error> {
        self.is_valid(Kind509::Leaf, &[], opts)?;
        let mut chains: Vec<Vec<&Certificate>> = if opts.roots.contains(self) {
            vec![vec![self]]
        } else {
            let mut checks = 0usize;
            build_chains(vec![self], &mut checks, opts)?
        };
        let any = opts.key_usages.contains(&Eku::Any);
        let usages = if opts.key_usages.is_empty() {
            vec![Eku::ServerAuth]
        } else {
            opts.key_usages.clone()
        };
        let mut incompatible = 0;
        let mut invalid_policies = 0;
        let mut hint: Option<String> = None;
        chains.retain(|c| {
            if !crate::x509_constraints::policies_valid(c) {
                invalid_policies += 1;
                return false;
            }
            if !any && !chain_allows(c, &usages) {
                incompatible += 1;
                return false;
            }
            if let Err(e) = crate::x509_constraints::check_chain_constraints(c) {
                hint.get_or_insert(e);
                return false;
            }
            true
        });
        if chains.is_empty() {
            if let Some(h) = hint {
                return Err(X509Error(format!(
                    "x509: a root or intermediate certificate is not authorized to sign for this name: {h}"
                )));
            }
            let mut details = Vec::new();
            if incompatible > 0 {
                if invalid_policies == 0 {
                    return Err(err("x509: certificate specifies an incompatible key usage"));
                }
                details.push(format!(
                    "{incompatible} candidate chains with incompatible key usage"
                ));
            }
            if invalid_policies > 0 {
                details.push(format!(
                    "{invalid_policies} candidate chains with invalid policies"
                ));
            }
            if details.is_empty() {
                return Err(err("x509: no valid chains built"));
            }
            return Err(X509Error(format!(
                "x509: no valid chains built: {}",
                details.join(", ")
            )));
        }
        Ok(chains)
    }
}

const MAX_SIGNATURE_CHECKS: usize = 100;

/// buildChains, its error kept as Go keeps it: the last candidate's validity or deeper
/// failure where no chain is built, else the first failure as the unknown authority's
/// hint.
fn build_chains<'c>(
    chain: Vec<&'c Certificate>,
    checks: &mut usize,
    opts: &Options<'c>,
) -> Result<Vec<Vec<&'c Certificate>>, X509Error> {
    let Some(&c) = chain.last() else {
        return Ok(Vec::new());
    };
    let mut chains = Vec::new();
    let mut error: Option<X509Error> = None;
    let mut hint: Option<(X509Error, &Certificate)> = None;
    let candidates: Vec<(Kind509, &Certificate)> = opts
        .roots
        .parents(c)
        .into_iter()
        .map(|p| (Kind509::Root, p))
        .chain(
            opts.intermediates
                .parents(c)
                .into_iter()
                .map(|p| (Kind509::Intermediate, p)),
        )
        .collect();
    for (kind, p) in candidates {
        if p.public_key == PublicKey::Unknown || p.already_in(&chain) {
            continue;
        }
        *checks += 1;
        if *checks > MAX_SIGNATURE_CHECKS {
            error = Some(err(
                "x509: signature check attempts limit reached while verifying certificate chain",
            ));
            continue;
        }
        if let Err(e) = c.check_signature_from(p) {
            if hint.is_none() {
                hint = Some((e, p));
            }
            continue;
        }
        match p.is_valid(kind, &chain, opts) {
            Err(e) => {
                if hint.is_none() {
                    hint = Some((e.clone(), p));
                }
                error = Some(e);
                continue;
            }
            Ok(()) => error = None,
        }
        let mut next = chain.clone();
        next.push(p);
        match kind {
            Kind509::Root => chains.push(next),
            _ => match build_chains(next, checks, opts) {
                Ok(mut more) => {
                    error = None;
                    chains.append(&mut more);
                }
                Err(e) => error = Some(e),
            },
        }
    }
    if !chains.is_empty() {
        return Ok(chains);
    }
    if let Some(e) = error {
        return Err(e);
    }
    Err(X509Error(match hint {
        Some((h, cert)) => {
            let mut name = common_name(&cert.subject);
            if name.is_empty() {
                name = organization(&cert.subject)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| format!("serial:{}", cert.serial_string()));
            }
            format!(
                "x509: certificate signed by unknown authority (possibly because of {} while trying to verify candidate authority certificate {})",
                shards_dockerfile::go::quote(h.0.as_bytes()),
                shards_dockerfile::go::quote(name.as_bytes())
            )
        }
        None => "x509: certificate signed by unknown authority".into(),
    }))
}

/// checkChainForKeyUsage.
fn chain_allows(chain: &[&Certificate], usages: &[Eku]) -> bool {
    if chain.is_empty() {
        return false;
    }
    let mut remaining: Vec<Option<Eku>> = usages.iter().copied().map(Some).collect();
    let mut left = remaining.len();
    for cert in chain.iter().rev() {
        if cert.ext_key_usage.is_empty() && cert.unknown_ext_key_usage.is_empty() {
            continue;
        }
        if cert.ext_key_usage.contains(&Eku::Any) {
            continue;
        }
        for slot in remaining.iter_mut() {
            let Some(u) = *slot else { continue };
            if !cert.ext_key_usage.contains(&u) {
                *slot = None;
                left -= 1;
                if left == 0 {
                    return false;
                }
            }
        }
    }
    true
}

/// Days helpers, re-exported for the modules that print certificate times.
pub fn civil_days(y: i64, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d)
}

pub fn month_days(m: u32, y: i64) -> u32 {
    days_in(m, y)
}
