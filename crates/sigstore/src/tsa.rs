//! RFC 3161 timestamps as sigstore-go verifies them against a timestamping authority
//! (root.SigstoreTimestampingAuthority.Verify): the response read as digitorus/timestamp
//! reads it (ParseResponse, Parse), its CMS SignedData as digitorus/pkcs7 reads and checks
//! it (BER made DER by its ber2der, the SignedData read with its parse errors ignored as
//! pkcs7 ignores them, signed attributes re-encoded as asn1.Marshal encodes them), then
//! timestamp-authority's VerifyTimestampResponse: the signer's chain to the authority's
//! root, its timestamping key usage, and the message's hash; each failure in their words.

use num_bigint::BigInt;

use crate::asn1::{self, Fields, Kind, Params, Value};
use crate::time::Time;
use crate::trusted_root::{Timestamp, TimestampingAuthority};
use crate::x509::{self, Certificate, Eku, Hash, Options, Pool, SigAlg};

/// ber2der's failures.
fn ber(s: &str) -> String {
    format!("ber2der: {s}")
}

/// A constructed object being read: its tag's octets, its DER contents so far, where its
/// definite contents end, and whether its length is indefinite.
struct Frame<'a> {
    tag: &'a [u8],
    content: Vec<u8>,
    end: usize,
    indefinite: bool,
}

/// encodeLength.
fn encode_length(out: &mut Vec<u8>, length: usize) {
    if length >= 128 {
        let bytes: Vec<u8> = length.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    } else {
        out.push(length as u8);
    }
}

/// readObject's header: the tag's octets, whether constructed, the length, whether
/// indefinite, and where the contents start.
fn header(b: &[u8], mut offset: usize) -> Result<(&[u8], bool, usize, bool, usize), String> {
    let len = b.len();
    let past = || ber("cannot move offset forward, end of ber data reached");
    let Some(&first) = b.get(offset) else {
        return Err(ber("offset is after end of ber data"));
    };
    let start = offset;
    offset += 1;
    if offset >= len {
        return Err(past());
    }
    if first & 0x1f == 0x1f {
        while b.get(offset).is_some_and(|x| *x >= 0x80) {
            offset += 1;
            if offset >= len {
                return Err(past());
            }
        }
        offset += 1;
        if offset >= len {
            return Err(past());
        }
    }
    let tag = b.get(start..offset).unwrap_or_default();
    let constructed = first & 0x20 != 0;
    let l = b.get(offset).copied().ok_or_else(past)?;
    offset += 1;
    if l >= 0x80 && offset >= len {
        return Err(past());
    }
    let mut length: usize = 0;
    let mut indefinite = false;
    if l > 0x80 {
        let n = usize::from(l & 0x7f);
        if n > 4 {
            return Err(ber("BER tag length too long"));
        }
        if n == 4 && b.get(offset).is_some_and(|x| *x > 0x7f) {
            return Err(ber("BER tag length is negative"));
        }
        if offset + n > len {
            return Err(past());
        }
        if b.get(offset) == Some(&0) && (n == 1 || b.get(offset + 1).is_some_and(|x| *x <= 0x7f)) {
            return Err(ber("BER tag length has leading zero"));
        }
        for _ in 0..n {
            length = length * 256 + usize::from(b.get(offset).copied().unwrap_or(0));
            offset += 1;
        }
    } else if l == 0x80 {
        indefinite = true;
    } else {
        length = usize::from(l);
    }
    if offset + length > len {
        return Err(ber("BER tag length is more than available data"));
    }
    if indefinite && !constructed {
        return Err(ber("Indefinite form tag must have constructed encoding"));
    }
    Ok((tag, constructed, length, indefinite, offset))
}

/// ber2der: the first object of `b` re-encoded with definite, shortest lengths (what
/// follows it dropped, as pkcs7 drops it), read without recursion.
pub fn ber2der(b: &[u8]) -> Result<Vec<u8>, String> {
    if b.is_empty() {
        return Err(ber("input ber is empty"));
    }
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut offset = 0;
    loop {
        // Read an object at `offset`; a primitive completes at once.
        let (tag, constructed, length, indefinite, start) = header(b, offset)?;
        let end = start + length;
        let mut done: Option<(Vec<u8>, usize)> = if constructed {
            stack.push(Frame {
                tag,
                content: Vec::new(),
                end,
                indefinite,
            });
            offset = start;
            if !indefinite && start >= end {
                // No children: the object completes now.
                stack.pop().map(|f| (wrap(&f), end))
            } else {
                None
            }
        } else {
            let mut out = tag.to_vec();
            encode_length(&mut out, length);
            out.extend_from_slice(b.get(start..end).unwrap_or_default());
            Some((out, end))
        };
        // Hand completed objects to their parents until one wants another child.
        while let Some((encoded, next)) = done.take() {
            let Some(parent) = stack.last_mut() else {
                return Ok(encoded);
            };
            parent.content.extend(encoded);
            offset = next;
            if parent.indefinite {
                if b.len().saturating_sub(offset) < 2 {
                    return Err(ber("Invalid BER format"));
                }
                if b.get(offset..offset + 2) == Some(&[0, 0]) {
                    let f = stack.pop().ok_or_else(|| ber("Invalid BER format"))?;
                    done = Some((wrap(&f), offset + 2));
                }
            } else if offset >= parent.end {
                let f = stack.pop().ok_or_else(|| ber("Invalid BER format"))?;
                let end = f.end;
                done = Some((wrap(&f), end));
            }
        }
    }
}

/// A constructed object's DER: its tag, the length of its contents, its contents.
fn wrap(f: &Frame<'_>) -> Vec<u8> {
    let mut out = f.tag.to_vec();
    encode_length(&mut out, f.content.len());
    out.extend_from_slice(&f.content);
    out
}

/// An OID's DER from its arcs, as asn1.Marshal writes it.
fn oid_der(arcs: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut push = |v: u64| {
        let mut parts = vec![(v & 0x7f) as u8];
        let mut v = v >> 7;
        while v > 0 {
            parts.push((v & 0x7f) as u8 | 0x80);
            v >>= 7;
        }
        body.extend(parts.iter().rev());
    };
    if let (Some(&a), Some(&b)) = (arcs.first(), arcs.get(1)) {
        push(u64::from(a) * 40 + u64::from(b));
    }
    for &a in arcs.iter().skip(2) {
        push(u64::from(a));
    }
    crate::der::tlv(crate::der::OID, &body)
}

fn oid_string(arcs: &[u32]) -> String {
    arcs.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

const OID_DATA_SIGNED: &[u32] = &[1, 2, 840, 113549, 1, 7, 2];
const OID_DATA_ENVELOPED: &[u32] = &[1, 2, 840, 113549, 1, 7, 3];
const OID_DATA_ENCRYPTED: &[u32] = &[1, 2, 840, 113549, 1, 7, 6];
const OID_MESSAGE_DIGEST: &[u32] = &[1, 2, 840, 113549, 1, 9, 4];
const OID_SIGNING_TIME: &[u32] = &[1, 2, 840, 113549, 1, 9, 5];
const OID_SHA1: &[u32] = &[1, 3, 14, 3, 2, 26];
const OID_SHA256: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];
const OID_SHA384: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 2];
const OID_SHA512: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 3];
const OID_DSA: &[u32] = &[1, 2, 840, 10040, 4, 1];
const OID_DSA_SHA1: &[u32] = &[1, 2, 840, 10040, 4, 3];
const OID_ECDSA_SHA1: &[u32] = &[1, 2, 840, 10045, 4, 1];
const OID_ECDSA_SHA256: &[u32] = &[1, 2, 840, 10045, 4, 3, 2];
const OID_ECDSA_SHA384: &[u32] = &[1, 2, 840, 10045, 4, 3, 3];
const OID_ECDSA_SHA512: &[u32] = &[1, 2, 840, 10045, 4, 3, 4];
const OID_RSA: &[u32] = &[1, 2, 840, 113549, 1, 1, 1];
const OID_RSA_SHA1: &[u32] = &[1, 2, 840, 113549, 1, 1, 5];
const OID_RSA_SHA256: &[u32] = &[1, 2, 840, 113549, 1, 1, 11];
const OID_RSA_SHA384: &[u32] = &[1, 2, 840, 113549, 1, 1, 12];
const OID_RSA_SHA512: &[u32] = &[1, 2, 840, 113549, 1, 1, 13];
const OID_EC_P256: &[u32] = &[1, 2, 840, 10045, 3, 1, 7];
const OID_EC_P384: &[u32] = &[1, 3, 132, 0, 34];
const OID_EC_P521: &[u32] = &[1, 3, 132, 0, 35];
const OID_ED25519: &[u32] = &[1, 3, 101, 112];

const UNSUPPORTED_ALGORITHM: &str =
    "pkcs7: cannot decrypt data: only RSA, DES, DES-EDE3, AES-256-CBC and AES-128-GCM supported";

/// A field's parameters, named as the Go type it is read into.
fn p(name: &'static str) -> Params {
    Params::default().named(name)
}

/// The value as a struct's contents.
fn inner<'a>(v: Option<Value<'a>>) -> &'a [u8] {
    match v {
        Some(Value::Struct { inner, .. }) => inner,
        _ => &[],
    }
}

fn oid_of(v: Option<Value<'_>>) -> Vec<u32> {
    match v {
        Some(Value::Oid(o)) => o,
        _ => Vec::new(),
    }
}

fn bytes_of<'a>(v: Option<Value<'a>>) -> &'a [u8] {
    match v {
        Some(Value::Bytes(b)) => b,
        _ => &[],
    }
}

fn list_of(v: Option<Value<'_>>) -> Vec<Value<'_>> {
    match v {
        Some(Value::List(l)) => l,
        _ => Vec::new(),
    }
}

/// pkix.AlgorithmIdentifier's fields: its OID.
fn algorithm_identifier(b: &[u8]) -> Result<Vec<u32>, String> {
    let mut f = Fields::new(b);
    let oid = oid_of(f.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?);
    f.next(Kind::Raw, &p("RawValue").optional()).map_err(|e| e.0)?;
    Ok(oid)
}

/// An attribute: its type, and its value's SET whole and contents.
#[derive(Debug, Clone)]
struct Attribute {
    oid: Vec<u32>,
    full: Vec<u8>,
    bytes: Vec<u8>,
}

fn attributes(list: Vec<Value<'_>>) -> Result<Vec<Attribute>, String> {
    let mut out = Vec::new();
    for v in list {
        let mut f = Fields::new(inner(Some(v)));
        let oid = oid_of(f.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?);
        let mut set = p("RawValue");
        set.set = true;
        match f.next(Kind::Raw, &set).map_err(|e| e.0)? {
            Some(Value::Raw { bytes, full, .. }) => out.push(Attribute {
                oid,
                full: full.to_vec(),
                bytes: bytes.to_vec(),
            }),
            _ => out.push(Attribute {
                oid,
                full: Vec::new(),
                bytes: Vec::new(),
            }),
        }
    }
    Ok(out)
}

/// A signerInfo.
#[derive(Debug, Clone)]
struct SignerInfo {
    issuer: Vec<u8>,
    serial: Vec<u8>,
    digest_algorithm: Vec<u32>,
    authenticated: Vec<Attribute>,
    encryption_algorithm: Vec<u32>,
    encrypted_digest: Vec<u8>,
}

fn signer_info(b: &[u8]) -> Result<SignerInfo, String> {
    let mut f = Fields::new(b);
    f.next(Kind::Int64, &p("int").with_default(1)).map_err(|e| e.0)?;
    let ias = inner(f.next(Kind::Struct, &p("issuerAndSerial")).map_err(|e| e.0)?);
    let mut g = Fields::new(ias);
    let issuer = match g.next(Kind::Raw, &p("RawValue")).map_err(|e| e.0)? {
        Some(Value::Raw { full, .. }) => full.to_vec(),
        _ => Vec::new(),
    };
    let serial = match g.next(Kind::BigInt, &p("")).map_err(|e| e.0)? {
        Some(Value::BigInt(s)) => s.to_vec(),
        _ => Vec::new(),
    };
    let digest_algorithm = algorithm_identifier(inner(
        f.next(Kind::Struct, &p("AlgorithmIdentifier")).map_err(|e| e.0)?,
    ))?;
    let mut auth = Params::tagged(0).optional().named("");
    auth.omit_empty = true;
    let authenticated = attributes(list_of(
        f.next(Kind::SeqOf(&Kind::Struct), &auth).map_err(|e| e.0)?,
    ))?;
    let encryption_algorithm = algorithm_identifier(inner(
        f.next(Kind::Struct, &p("AlgorithmIdentifier")).map_err(|e| e.0)?,
    ))?;
    let encrypted_digest = bytes_of(f.next(Kind::Bytes, &p("")).map_err(|e| e.0)?).to_vec();
    let mut unauth = Params::tagged(1).optional().named("");
    unauth.omit_empty = true;
    attributes(list_of(
        f.next(Kind::SeqOf(&Kind::Struct), &unauth).map_err(|e| e.0)?,
    ))?;
    Ok(SignerInfo {
        issuer,
        serial,
        digest_algorithm,
        authenticated,
        encryption_algorithm,
        encrypted_digest,
    })
}

/// pkix.Extension.
fn extension(b: &[u8]) -> Result<(), String> {
    let mut f = Fields::new(b);
    f.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?;
    f.next(Kind::Bool, &p("bool").optional()).map_err(|e| e.0)?;
    f.next(Kind::Bytes, &p("")).map_err(|e| e.0)?;
    Ok(())
}

fn extensions(list: Vec<Value<'_>>) -> Result<(), String> {
    for v in list {
        extension(inner(Some(v)))?;
    }
    Ok(())
}

/// pkix.CertificateList, read only for its errors.
fn certificate_list(b: &[u8]) -> Result<(), String> {
    let mut f = Fields::new(b);
    let tbs = inner(f.next(Kind::Struct, &p("TBSCertificateList")).map_err(|e| e.0)?);
    let mut t = Fields::new(tbs);
    t.next(Kind::Int64, &p("int").optional().with_default(0))
        .map_err(|e| e.0)?;
    algorithm_identifier(inner(
        t.next(Kind::Struct, &p("AlgorithmIdentifier")).map_err(|e| e.0)?,
    ))?;
    static RDN: Kind = Kind::SetOf(&Kind::Struct);
    for rdn in list_of(t.next(Kind::SeqOf(&RDN), &p("RDNSequence")).map_err(|e| e.0)?) {
        for atv in list_of(Some(rdn)) {
            let mut a = Fields::new(inner(Some(atv)));
            a.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?;
            a.next(Kind::Any, &p("")).map_err(|e| e.0)?;
        }
    }
    t.next(Kind::Time, &p("Time")).map_err(|e| e.0)?;
    t.next(Kind::Time, &p("Time").optional()).map_err(|e| e.0)?;
    for rc in list_of(
        t.next(Kind::SeqOf(&Kind::Struct), &p("").optional())
            .map_err(|e| e.0)?,
    ) {
        let mut r = Fields::new(inner(Some(rc)));
        r.next(Kind::BigInt, &p("")).map_err(|e| e.0)?;
        r.next(Kind::Time, &p("Time")).map_err(|e| e.0)?;
        extensions(list_of(
            r.next(Kind::SeqOf(&Kind::Struct), &p("").optional())
                .map_err(|e| e.0)?,
        ))?;
    }
    let ext = Params::explicit(0).optional().named("");
    extensions(list_of(
        t.next(Kind::SeqOf(&Kind::Struct), &ext).map_err(|e| e.0)?,
    ))?;
    algorithm_identifier(inner(
        f.next(Kind::Struct, &p("AlgorithmIdentifier")).map_err(|e| e.0)?,
    ))?;
    f.next(Kind::BitString, &p("BitString")).map_err(|e| e.0)?;
    Ok(())
}

/// A parsed PKCS #7 message.
#[derive(Debug, Clone, Default)]
struct Pkcs7 {
    content: Vec<u8>,
    /// None: no certificates (Go's nil).
    certificates: Option<Vec<Certificate>>,
    signers: Vec<SignerInfo>,
}

/// What signedData's fields came to before its first error (which pkcs7 ignores).
#[derive(Default)]
struct SignedData<'a> {
    content: &'a [u8],
    certificates: &'a [u8],
    signers: Vec<SignerInfo>,
}

fn signed_data<'a>(data: &'a [u8], sd: &mut SignedData<'a>) -> Result<(), String> {
    let (top, _) = asn1::unmarshal(data, Kind::Struct, &p("signedData")).map_err(|e| e.0)?;
    let mut f = Fields::new(inner(Some(top)));
    f.next(Kind::Int64, &p("int").with_default(1)).map_err(|e| e.0)?;
    let mut set = p("");
    set.set = true;
    for v in list_of(f.next(Kind::SeqOf(&Kind::Struct), &set).map_err(|e| e.0)?) {
        algorithm_identifier(inner(Some(v)))?;
    }
    let ci = inner(f.next(Kind::Struct, &p("contentInfo")).map_err(|e| e.0)?);
    let mut c = Fields::new(ci);
    c.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?;
    if let Some(Value::Raw { bytes, .. }) = c
        .next(Kind::Raw, &Params::explicit(0).optional().named("RawValue"))
        .map_err(|e| e.0)?
    {
        sd.content = bytes;
    }
    if let Some(Value::Struct { full, .. }) = f
        .next(
            Kind::Struct,
            &Params::tagged(0).optional().named("rawCertificates"),
        )
        .map_err(|e| e.0)?
    {
        sd.certificates = full;
    }
    for v in list_of(
        f.next(
            Kind::SeqOf(&Kind::Struct),
            &Params::tagged(1).optional().named(""),
        )
        .map_err(|e| e.0)?,
    ) {
        certificate_list(inner(Some(v)))?;
    }
    let mut signers = Vec::new();
    for v in list_of(f.next(Kind::SeqOf(&Kind::Struct), &set).map_err(|e| e.0)?) {
        signers.push(signer_info(inner(Some(v)))?);
    }
    sd.signers = signers;
    Ok(())
}

/// parseSignedData.
fn parse_signed_data(data: &[u8]) -> Result<Pkcs7, String> {
    let mut sd = SignedData::default();
    // pkcs7 ignores the error: the fields read before it stand.
    let _ = signed_data(data, &mut sd);
    let certificates = if sd.certificates.is_empty() {
        None
    } else {
        let (raw, _) = asn1::unmarshal(sd.certificates, Kind::Raw, &p("RawValue")).map_err(|e| e.0)?;
        let Value::Raw { bytes, .. } = raw else {
            return Err("asn1: syntax error: sequence truncated".into());
        };
        let certs = Certificate::parse_all(bytes).map_err(|e| e.0)?;
        if certs.is_empty() { None } else { Some(certs) }
    };
    let mut content: Vec<u8> = Vec::new();
    if !sd.content.is_empty() {
        let (compound, _) = asn1::unmarshal(sd.content, Kind::Raw, &p("RawValue")).map_err(|e| e.0)?;
        if let Value::Raw {
            compound: true,
            tag: 4,
            bytes,
            ..
        } = compound
        {
            let (v, _) = asn1::unmarshal(bytes, Kind::Bytes, &p("unsignedData")).map_err(|e| e.0)?;
            content = bytes_of(Some(v)).to_vec();
        } else if let Value::Raw { bytes, .. } = compound {
            content = bytes.to_vec();
        }
    }
    Ok(Pkcs7 {
        content,
        certificates,
        signers: sd.signers,
    })
}

/// encryptedContentInfo, read for its errors.
fn encrypted_content_info(b: &[u8]) -> Result<(), String> {
    let mut f = Fields::new(b);
    f.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?;
    algorithm_identifier(inner(
        f.next(Kind::Struct, &p("AlgorithmIdentifier")).map_err(|e| e.0)?,
    ))?;
    f.next(Kind::Raw, &Params::tagged(0).optional().named("RawValue"))
        .map_err(|e| e.0)?;
    Ok(())
}

/// pkcs7.Parse.
fn pkcs7_parse(data: &[u8]) -> Result<Pkcs7, String> {
    if data.is_empty() {
        return Err("pkcs7: input data is empty".into());
    }
    let der = ber2der(data)?;
    let (info, rest) = asn1::unmarshal(&der, Kind::Struct, &p("contentInfo")).map_err(|e| e.0)?;
    let mut f = Fields::new(inner(Some(info)));
    let content_type = oid_of(f.next(Kind::Oid, &p("ObjectIdentifier")).map_err(|e| e.0)?);
    let content = match f
        .next(Kind::Raw, &Params::explicit(0).optional().named("RawValue"))
        .map_err(|e| e.0)?
    {
        Some(Value::Raw { bytes, .. }) => bytes,
        _ => &[],
    };
    if !rest.is_empty() {
        return Err("asn1: syntax error: trailing data".into());
    }
    if content_type == OID_DATA_SIGNED {
        return parse_signed_data(content);
    }
    if content_type == OID_DATA_ENVELOPED {
        let (v, _) = asn1::unmarshal(content, Kind::Struct, &p("envelopedData")).map_err(|e| e.0)?;
        let mut e = Fields::new(inner(Some(v)));
        e.next(Kind::Int64, &p("int")).map_err(|e| e.0)?;
        let mut set = p("");
        set.set = true;
        for r in list_of(e.next(Kind::SeqOf(&Kind::Struct), &set).map_err(|e| e.0)?) {
            let mut ri = Fields::new(inner(Some(r)));
            ri.next(Kind::Int64, &p("int")).map_err(|e| e.0)?;
            let ias = inner(ri.next(Kind::Struct, &p("issuerAndSerial")).map_err(|e| e.0)?);
            let mut g = Fields::new(ias);
            g.next(Kind::Raw, &p("RawValue")).map_err(|e| e.0)?;
            g.next(Kind::BigInt, &p("")).map_err(|e| e.0)?;
            algorithm_identifier(inner(
                ri.next(Kind::Struct, &p("AlgorithmIdentifier"))
                    .map_err(|e| e.0)?,
            ))?;
            ri.next(Kind::Bytes, &p("")).map_err(|e| e.0)?;
        }
        encrypted_content_info(inner(
            e.next(Kind::Struct, &p("encryptedContentInfo"))
                .map_err(|e| e.0)?,
        ))?;
        return Ok(Pkcs7::default());
    }
    if content_type == OID_DATA_ENCRYPTED {
        let (v, _) = asn1::unmarshal(content, Kind::Struct, &p("encryptedData")).map_err(|e| e.0)?;
        let mut e = Fields::new(inner(Some(v)));
        e.next(Kind::Int64, &p("int")).map_err(|e| e.0)?;
        encrypted_content_info(inner(
            e.next(Kind::Struct, &p("encryptedContentInfo"))
                .map_err(|e| e.0)?,
        ))?;
        return Ok(Pkcs7::default());
    }
    Err("pkcs7: cannot parse data: unimplemented content type".into())
}

/// A two's complement integer's value.
fn int_of(b: &[u8]) -> BigInt {
    BigInt::from_signed_bytes_be(b)
}

/// getCertFromCertsByIssuerAndSerial.
fn signer_cert<'c>(certs: &'c [Certificate], s: &SignerInfo) -> Option<&'c Certificate> {
    certs
        .iter()
        .find(|c| int_of(&c.serial) == int_of(&s.serial) && c.raw_issuer == s.issuer)
}

/// getHashForOID.
fn hash_for(oid: &[u32]) -> Result<Hash, String> {
    match oid {
        OID_SHA1 | OID_ECDSA_SHA1 | OID_DSA | OID_DSA_SHA1 | OID_RSA => Ok(Hash::Sha1),
        OID_SHA256 | OID_ECDSA_SHA256 => Ok(Hash::Sha256),
        OID_SHA384 | OID_ECDSA_SHA384 => Ok(Hash::Sha384),
        OID_SHA512 | OID_ECDSA_SHA512 => Ok(Hash::Sha512),
        _ => Err(UNSUPPORTED_ALGORITHM.into()),
    }
}

/// getSignatureAlgorithm.
fn signature_algorithm(encryption: &[u32], digest: &[u32]) -> Result<SigAlg, String> {
    let unsupported = || {
        format!(
            "pkcs7: unsupported digest \"{}\" for encryption algorithm \"{}\"",
            oid_string(digest),
            oid_string(encryption)
        )
    };
    match encryption {
        OID_ECDSA_SHA1 => Ok(SigAlg::EcdsaSha1),
        OID_ECDSA_SHA256 => Ok(SigAlg::EcdsaSha256),
        OID_ECDSA_SHA384 => Ok(SigAlg::EcdsaSha384),
        OID_ECDSA_SHA512 => Ok(SigAlg::EcdsaSha512),
        OID_RSA | OID_RSA_SHA1 | OID_RSA_SHA256 | OID_RSA_SHA384 | OID_RSA_SHA512 => match digest {
            OID_SHA1 => Ok(SigAlg::Sha1Rsa),
            OID_SHA256 => Ok(SigAlg::Sha256Rsa),
            OID_SHA384 => Ok(SigAlg::Sha384Rsa),
            OID_SHA512 => Ok(SigAlg::Sha512Rsa),
            _ => Err(unsupported()),
        },
        OID_DSA | OID_DSA_SHA1 => match digest {
            OID_SHA1 => Ok(SigAlg::DsaSha1),
            OID_SHA256 => Ok(SigAlg::DsaSha256),
            _ => Err(unsupported()),
        },
        OID_EC_P256 | OID_EC_P384 | OID_EC_P521 => match digest {
            OID_SHA1 => Ok(SigAlg::EcdsaSha1),
            OID_SHA256 => Ok(SigAlg::EcdsaSha256),
            OID_SHA384 => Ok(SigAlg::EcdsaSha384),
            OID_SHA512 => Ok(SigAlg::EcdsaSha512),
            _ => Err(unsupported()),
        },
        OID_ED25519 => Ok(SigAlg::Ed25519),
        _ => Err(format!(
            "pkcs7: unsupported algorithm \"{}\"",
            oid_string(encryption)
        )),
    }
}

/// marshalAttributes: the attributes as a DER SET OF, sorted as asn1.Marshal sorts one.
fn marshal_attributes(attrs: &[Attribute]) -> Vec<u8> {
    let mut encoded: Vec<Vec<u8>> = attrs
        .iter()
        .map(|a| crate::der::tlv(crate::der::SEQUENCE, &[oid_der(&a.oid), a.full.clone()].concat()))
        .collect();
    if encoded.len() > 1 {
        encoded.sort();
    }
    crate::der::tlv(crate::der::SET, &encoded.concat())
}

/// verifySignature, with no roots: the signed attributes and the signature.
fn verify_signer(p7: &Pkcs7, s: &SignerInfo) -> Result<(), String> {
    let certs = p7.certificates.as_deref().unwrap_or_default();
    let ee = signer_cert(certs, s).ok_or("pkcs7: No certificate for signer")?;
    let mut signed = p7.content.clone();
    if !s.authenticated.is_empty() {
        let attr = |oid: &[u32]| s.authenticated.iter().find(|a| a.oid == oid);
        let digest = match attr(OID_MESSAGE_DIGEST) {
            Some(a) => {
                let (v, _) = asn1::unmarshal(&a.bytes, Kind::Bytes, &p("")).map_err(|e| e.0)?;
                bytes_of(Some(v)).to_vec()
            }
            None => return Err("pkcs7: attribute type not in attributes".into()),
        };
        let hash = hash_for(&s.digest_algorithm)?;
        let computed = hash.of(&p7.content);
        if digest != computed {
            let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02X}")).collect::<String>();
            return Err(format!(
                "pkcs7: Message digest mismatch\n\tExpected: {}\n\tActual  : {}",
                hex(&digest),
                hex(&computed)
            ));
        }
        signed = marshal_attributes(&s.authenticated);
        if let Some(a) = attr(OID_SIGNING_TIME)
            && let Ok((Value::Time(t), _)) = asn1::unmarshal(&a.bytes, Kind::Time, &p("Time"))
            && (t > ee.not_after || t < ee.not_before)
        {
            return Err(format!(
                "pkcs7: signing time \"{}\" is outside of certificate validity \"{}\" to \"{}\"",
                t.rfc3339(),
                ee.not_before.rfc3339(),
                ee.not_after.rfc3339()
            ));
        }
    }
    let alg = signature_algorithm(&s.encryption_algorithm, &s.digest_algorithm)?;
    ee.check_signature(alg, &signed, &s.encrypted_digest)
        .map_err(|e| e.0)
}

/// PKCS7.Verify: every signer.
fn pkcs7_verify(p7: &Pkcs7) -> Result<(), String> {
    if p7.signers.is_empty() {
        return Err("pkcs7: Message has no signers".into());
    }
    for s in &p7.signers {
        verify_signer(p7, s)?;
    }
    Ok(())
}

/// A failure of the response's parse: digitorus' ParseError, or any other.
enum ParseFailure {
    Parse(String),
    Other(String),
}

/// A parsed timestamp.
struct Token {
    raw: Vec<u8>,
    hashed_message: Vec<u8>,
    time: Time,
    hash: Hash,
}

/// timestamp.Parse.
fn parse_token(bytes: &[u8]) -> Result<Token, ParseFailure> {
    let p7 = pkcs7_parse(bytes).map_err(ParseFailure::Other)?;
    if p7.certificates.is_some() {
        pkcs7_verify(&p7).map_err(ParseFailure::Other)?;
    }
    let other = |e: asn1::Asn1Error| ParseFailure::Other(e.0);
    let (tst, _) = asn1::unmarshal(&p7.content, Kind::Struct, &p("tstInfo")).map_err(other)?;
    let mut f = Fields::new(inner(Some(tst)));
    f.next(Kind::Int64, &p("int")).map_err(other)?;
    f.next(Kind::Oid, &p("ObjectIdentifier")).map_err(other)?;
    let mi = inner(f.next(Kind::Struct, &p("messageImprint")).map_err(other)?);
    let mut m = Fields::new(mi);
    let ha = inner(m.next(Kind::Struct, &p("AlgorithmIdentifier")).map_err(other)?);
    let mut a = Fields::new(ha);
    let hash_oid = oid_of(a.next(Kind::Oid, &p("ObjectIdentifier")).map_err(other)?);
    a.next(Kind::Raw, &p("RawValue").optional()).map_err(other)?;
    let hashed_message = bytes_of(m.next(Kind::Bytes, &p("")).map_err(other)?).to_vec();
    f.next(Kind::BigInt, &p("")).map_err(other)?;
    let mut time_params = p("Time");
    time_params.time_type = asn1::TAG_GENERALIZED_TIME;
    let time = match f.next(Kind::Time, &time_params).map_err(other)? {
        Some(Value::Time(t)) => t,
        _ => Time::utc(crate::time::ZERO_SECS, 0),
    };
    if let Some(acc) = f.next(Kind::Struct, &p("accuracy").optional()).map_err(other)? {
        let mut g = Fields::new(inner(Some(acc)));
        g.next(Kind::Int64, &p("int64").optional()).map_err(other)?;
        g.next(Kind::Int64, &Params::tagged(0).optional().named("int64"))
            .map_err(other)?;
        g.next(Kind::Int64, &Params::tagged(1).optional().named("int64"))
            .map_err(other)?;
    }
    f.next(Kind::Bool, &p("bool").optional()).map_err(other)?;
    f.next(Kind::BigInt, &p("").optional()).map_err(other)?;
    f.next(Kind::Raw, &Params::tagged(0).optional().named("RawValue"))
        .map_err(other)?;
    let exts = list_of(
        f.next(
            Kind::SeqOf(&Kind::Struct),
            &Params::tagged(1).optional().named(""),
        )
        .map_err(other)?,
    );
    extensions(exts).map_err(ParseFailure::Other)?;
    if hashed_message.is_empty() {
        return Err(ParseFailure::Parse(
            "Time-Stamp response contains no hashed message".into(),
        ));
    }
    let hash = match hash_oid.as_slice() {
        OID_SHA1 => Hash::Sha1,
        OID_SHA256 => Hash::Sha256,
        OID_SHA384 => Hash::Sha384,
        OID_SHA512 => Hash::Sha512,
        _ => {
            return Err(ParseFailure::Parse(
                "Time-Stamp response uses unknown hash function".into(),
            ));
        }
    };
    Ok(Token {
        raw: bytes.to_vec(),
        hashed_message,
        time,
        hash,
    })
}

/// Status.String.
fn status_string(s: i64) -> String {
    match s {
        0 => "the request is granted".into(),
        1 => "the request is granted with modifications".into(),
        2 => "the request is rejected".into(),
        3 => "the request is waiting".into(),
        4 => "revocation is imminent".into(),
        5 => "revocation has occurred".into(),
        _ => format!("unknown status: {s}"),
    }
}

/// FailureInfo.String of the first failure bit set, "" where none is.
fn failure_info(bits: &[u8], bit_length: usize) -> &'static str {
    let at = |i: usize| i < bit_length && bits.get(i / 8).is_some_and(|b| (b >> (7 - i % 8)) & 1 == 1);
    for (i, s) in [
        (0, "unrecognized or unsupported Algorithm Identifier"),
        (2, "transaction not permitted or supported"),
        (5, "the data submitted has the wrong format"),
        (14, "the TSA's time source is not available"),
        (15, "the requested TSA policy is not supported by the TSA"),
        (16, "the requested extension is not supported by the TSA"),
        (
            17,
            "the additional information requested could not be understood or is not available",
        ),
        (25, "the request cannot be handled due to system failure"),
    ] {
        if at(i) {
            return s;
        }
    }
    ""
}

/// timestamp.ParseResponse.
fn parse_response(bytes: &[u8]) -> Result<Token, ParseFailure> {
    let other = |e: asn1::Asn1Error| ParseFailure::Other(e.0);
    let (resp, rest) = asn1::unmarshal(bytes, Kind::Struct, &p("response")).map_err(other)?;
    let mut f = Fields::new(inner(Some(resp)));
    let status = inner(f.next(Kind::Struct, &p("pkiStatusInfo")).map_err(other)?);
    let mut s = Fields::new(status);
    let code = match s.next(Kind::Int64, &p("Status")).map_err(other)? {
        Some(Value::Int(i)) => i,
        _ => 0,
    };
    static STR: Kind = Kind::Str;
    let mut strings_params = p("").optional();
    strings_params.string_type = asn1::TAG_UTF8_STRING;
    let strings: Vec<String> = list_of(s.next(Kind::SeqOf(&STR), &strings_params).map_err(other)?)
        .into_iter()
        .filter_map(|v| match v {
            Value::Str(s) => Some(s),
            _ => None,
        })
        .collect();
    let (fail_bits, fail_len) = match s
        .next(Kind::BitString, &p("BitString").optional())
        .map_err(other)?
    {
        Some(Value::BitString { bytes, bit_length }) => (bytes.to_vec(), bit_length),
        _ => (Vec::new(), 0),
    };
    let token = match f.next(Kind::Raw, &p("RawValue").optional()).map_err(other)? {
        Some(Value::Raw { bytes, full, .. }) => (bytes.to_vec(), full.to_vec()),
        _ => (Vec::new(), Vec::new()),
    };
    if !rest.is_empty() {
        return Err(ParseFailure::Parse("trailing data in Time-Stamp response".into()));
    }
    if code > 0 {
        return Err(ParseFailure::Other(format!(
            "{}: {} ({})",
            status_string(code),
            strings.join(","),
            failure_info(&fail_bits, fail_len)
        )));
    }
    if token.0.is_empty() {
        return Err(ParseFailure::Parse("no pkcs7 data in Time-Stamp response".into()));
    }
    parse_token(&token.1)
}

/// verifyLeafCertCriticalEKU, verifyESSCertID, and the EKU of the leaf and the chain's
/// intermediates (verifyLeafCert).
fn verify_leaf(
    leaf: &Certificate,
    chains: &[Vec<&Certificate>],
    tsa_cert: Option<&Certificate>,
) -> Result<(), String> {
    let critical = leaf
        .extensions
        .iter()
        .find(|e| e.oid == x509::OID_EKU)
        .is_some_and(|e| e.critical);
    if !critical {
        return Err("failed to verify TSA certificate: certificate must set EKU to critical".into());
    }
    if let Some(t) = tsa_cert {
        if t.raw_issuer != leaf.raw_issuer {
            return Err(
                "failed to verify TSA certificate: TSR cert issuer does not match provided TSA cert issuer"
                    .into(),
            );
        }
        if int_of(&t.serial) != int_of(&leaf.serial) {
            return Err(
                "failed to verify TSA certificate: TSR cert serial number does not match provided TSA cert serial number"
                    .into(),
            );
        }
    }
    let eku = |e: String| {
        format!("failed to verify EKU on leaf certificate: failed to verify EKU on leaf certificate: {e}")
    };
    if leaf.ext_key_usage.len() != 1 {
        return Err(eku(format!(
            "certificate has {} extended key usages, expected only one",
            leaf.ext_key_usage.len()
        )));
    }
    if leaf.ext_key_usage.first() != Some(&Eku::TimeStamping) {
        return Err(eku(
            "leaf certificate EKU is not set to TimeStamping as required".into()
        ));
    }
    let mut last: Option<&str> = None;
    for chain in chains {
        let mut ok = true;
        for c in chain.iter().skip(1).take(chain.len().saturating_sub(2)) {
            if !c.ext_key_usage.is_empty()
                && !c
                    .ext_key_usage
                    .iter()
                    .any(|u| matches!(u, Eku::TimeStamping | Eku::Any))
            {
                ok = false;
                last = Some("intermediate certificate does not allow Timestamping usage");
                break;
            }
        }
        if ok {
            return Ok(());
        }
    }
    if let Some(e) = last {
        return Err(format!(
            "failed to verify EKU on leaf certificate: no verified certificate chain met EKU requirements: {e}"
        ));
    }
    Ok(())
}

/// VerifyTimestampResponse, with the authority's root, intermediates and leaf.
fn verify_response(
    tsa: &TimestampingAuthority,
    root: &Certificate,
    signed: &[u8],
    signature: &[u8],
) -> Result<Time, String> {
    let ts = match parse_response(signed) {
        Ok(ts) => ts,
        Err(ParseFailure::Parse(e)) => return Err(format!("timestamp response is not valid: {e}")),
        Err(ParseFailure::Other(e)) => return Err(format!("error parsing response into Timestamp: {e}")),
    };
    match ts.hash {
        Hash::Sha1 => return Err("weak hash algorithm: must be SHA-256, SHA-384, or SHA-512".into()),
        Hash::Sha256 | Hash::Sha384 | Hash::Sha512 => {}
        _ => return Err("unsupported hash algorithm".into()),
    }
    // verifyTSRWithChain.
    let mut p7 = pkcs7_parse(&ts.raw).map_err(|e| format!("error parsing hashed message: {e}"))?;
    if p7.certificates.is_none() && tsa.leaf.is_none() {
        return Err("leaf certificate must be present in the TSR or as a verify option".into());
    }
    let mut roots = Pool::default();
    roots.add(root.clone());
    let mut intermediates = Pool::default();
    for c in &tsa.intermediates {
        intermediates.add(c.clone());
    }
    for c in p7.certificates.iter().flatten() {
        intermediates.add(c.clone());
    }
    if p7.certificates.is_none()
        && let Some(leaf) = &tsa.leaf
    {
        p7.certificates = Some(vec![leaf.clone()]);
    }
    pkcs7_verify(&p7).map_err(|e| format!("error while verifying signature: {e}"))?;
    let signer = match p7.signers.as_slice() {
        [s] => signer_cert(p7.certificates.as_deref().unwrap_or_default(), s),
        _ => None,
    }
    .ok_or("signer certificate was not found")?;
    if let Some(leaf) = &tsa.leaf
        && leaf.raw != signer.raw
    {
        return Err("certificate embedded in the TSR does not match the provided TSA certificate".into());
    }
    let opts = Options {
        roots: &roots,
        intermediates: &intermediates,
        now: ts.time,
        key_usages: vec![Eku::TimeStamping],
    };
    let chains = signer
        .verify(&opts)
        .map_err(|e| format!("error while verifying signer certificate chain: {}", e.0))?;
    verify_leaf(signer, &chains, tsa.leaf.as_ref())?;
    if ts.hash.of(signature) != ts.hashed_message {
        return Err("hashed messages don't match".into());
    }
    Ok(ts.time)
}

/// SigstoreTimestampingAuthority.Verify: the time `signed_timestamp` vouches for
/// `signature` at, by this authority.
pub fn verify(
    tsa: &TimestampingAuthority,
    signed_timestamp: &[u8],
    signature: &[u8],
) -> Result<Timestamp, String> {
    let Some(root) = &tsa.root else {
        let uri = if tsa.uri.is_empty() {
            String::new()
        } else {
            format!("{} ", tsa.uri)
        };
        return Err(format!("timestamping authority {uri}root certificate is nil"));
    };
    let time = verify_response(tsa, root, signed_timestamp, signature)?;
    if tsa.start.is_some_and(|s| time < s) {
        return Err("timestamp is before the validity period start".into());
    }
    if tsa.end.is_some_and(|e| time > e) {
        return Err("timestamp is after the validity period end".into());
    }
    Ok(Timestamp {
        time,
        uri: tsa.uri.clone(),
    })
}
