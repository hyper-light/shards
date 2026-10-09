//! Signed certificate timestamps as sigstore-go v1.2.2 verifies a Fulcio certificate's
//! (verify/sct.go), with what it calls from certificate-transparency-go (as buildx
//! v0.37.1 vendors it): the leaf read again by the CT fork of crypto/x509 and its fork
//! of encoding/asn1 (x509/x509.go `ParseCertificate`, whose non-fatal errors fail the
//! check as sigstore-go reads them), the SCT list in its TLS encoding (tls/tls.go), the
//! precertificate rebuilt as `RemoveSCTList` re-marshals the TBSCertificate, the signed
//! input (`SerializeSCTSignatureInput`) and `tls.VerifySignature` under
//! `ct.NewSignatureVerifier`'s key rules.
//!
//! Left as D105 records: SCT signatures over MD5 are refused (Go accepts them); RSAES-OAEP
//! keys, which CT's x509 parses and Go's leaves unknown, are not
//! (such a key cannot sign, so no chain carries one); and where encoding/asn1 prints a
//! pointer (`%+v` of a tag or default in a mismatch), `(ptr)` stands in for the address, as in `asn1`.

use std::collections::BTreeMap;

use crate::gotime;
use crate::time::Time;
use crate::trusted_root::TransparencyLog;
use crate::x509::{Certificate, Curve, PublicKey};

// ---------------------------------------------------------------------------------------
// certificate-transparency-go/asn1: encoding/asn1 with field names in its errors and a
// "lax" mode.

fn syn(msg: &str, name: &str) -> String {
    if name.is_empty() {
        format!("asn1: syntax error: {msg}")
    } else {
        format!("asn1: syntax error: {name}: {msg}")
    }
}

fn st(msg: &str, name: &str) -> String {
    if name.is_empty() {
        format!("asn1: structure error: {msg}")
    } else {
        format!("asn1: structure error: {name}: {msg}")
    }
}

const UNIVERSAL: u8 = 0;
const CONTEXT: u8 = 2;

const TAG_BOOLEAN: i64 = 1;
const TAG_INTEGER: i64 = 2;
const TAG_BIT_STRING: i64 = 3;
const TAG_OCTET_STRING: i64 = 4;
const TAG_OID: i64 = 6;
const TAG_UTF8: i64 = 12;
const TAG_SEQUENCE: i64 = 16;
const TAG_SET: i64 = 17;
const TAG_NUMERIC: i64 = 18;
const TAG_PRINTABLE: i64 = 19;
const TAG_T61: i64 = 20;
const TAG_IA5: i64 = 22;
const TAG_UTC_TIME: i64 = 23;
const TAG_GENERALIZED_TIME: i64 = 24;
const TAG_GENERAL_STRING: i64 = 27;
const TAG_BMP: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tl {
    class: u8,
    tag: i64,
    length: usize,
    compound: bool,
}

/// parseBase128Int (no minimality check in this fork).
fn base128(b: &[u8], mut offset: usize, name: &str) -> Result<(i64, usize), String> {
    let mut ret: i64 = 0;
    let mut shifted = 0;
    while let Some(&x) = b.get(offset) {
        if shifted == 5 {
            return Err(st("base 128 integer too large", name));
        }
        ret = (ret << 7) | i64::from(x & 0x7f);
        offset += 1;
        if x & 0x80 == 0 {
            if ret > i64::from(i32::MAX) {
                return Err(st("base 128 integer too large", name));
            }
            return Ok((ret, offset));
        }
        shifted += 1;
    }
    Err(syn("truncated base 128 integer", name))
}

/// parseTagAndLength.
fn tag_and_length(b: &[u8], offset: usize, name: &str) -> Result<(Tl, usize), String> {
    let Some(&first) = b.get(offset) else {
        return Err("asn1: internal error in parseTagAndLength".into());
    };
    let mut offset = offset + 1;
    let mut t = Tl {
        class: first >> 6,
        compound: first & 0x20 == 0x20,
        tag: i64::from(first & 0x1f),
        length: 0,
    };
    if t.tag == 0x1f {
        let (v, o) = base128(b, offset, name)?;
        t.tag = v;
        offset = o;
        if t.tag < 0x1f {
            return Err(syn("non-minimal tag", name));
        }
    }
    let Some(&lb) = b.get(offset) else {
        return Err(syn("truncated tag or length", name));
    };
    offset += 1;
    if lb & 0x80 == 0 {
        t.length = usize::from(lb & 0x7f);
    } else {
        let n = usize::from(lb & 0x7f);
        if n == 0 {
            return Err(syn("indefinite length found (not DER)", name));
        }
        let mut length: usize = 0;
        for _ in 0..n {
            let Some(&x) = b.get(offset) else {
                return Err(syn("truncated tag or length", name));
            };
            offset += 1;
            if length >= 1 << 23 {
                return Err(st("length too large", name));
            }
            length = (length << 8) | usize::from(x);
            if length == 0 {
                return Err(st("superfluous leading zeros in length", name));
            }
        }
        if length < 0x80 {
            return Err(st("non-minimal length", name));
        }
        t.length = length;
    }
    Ok((t, offset))
}

/// A Go type this fork reads into.
#[derive(Debug, Clone, Copy)]
enum Ty {
    /// asn1.RawValue.
    Raw,
    /// asn1.ObjectIdentifier.
    Oid,
    /// asn1.BitString.
    Bits,
    /// time.Time.
    Time,
    /// *big.Int.
    Big,
    /// int (an int64 here).
    Int,
    Bool,
    /// []byte.
    Bytes,
    /// interface{}.
    Any,
    /// A struct: its fields and its Go type name.
    Struct(&'static [Field], &'static str),
    /// A slice of a type: its element, whether its name ends in SET, and its Go type name.
    SeqOf(&'static Ty, bool, &'static str),
}

impl Ty {
    fn go_name(&self) -> &'static str {
        match self {
            Ty::Raw => "RawValue",
            Ty::Oid => "ObjectIdentifier",
            Ty::Bits => "BitString",
            Ty::Time => "Time",
            Ty::Int => "int",
            Ty::Bool => "bool",
            Ty::Big | Ty::Bytes | Ty::Any => "",
            Ty::Struct(_, n) | Ty::SeqOf(_, _, n) => n,
        }
    }

    /// getUniversalType: (matchAny, tag, compound).
    fn universal(&self) -> (bool, i64, bool) {
        match self {
            Ty::Raw | Ty::Any => (true, -1, false),
            Ty::Oid => (false, TAG_OID, false),
            Ty::Bits => (false, TAG_BIT_STRING, false),
            Ty::Time => (false, TAG_UTC_TIME, false),
            Ty::Big | Ty::Int => (false, TAG_INTEGER, false),
            Ty::Bool => (false, TAG_BOOLEAN, false),
            Ty::Bytes => (false, TAG_OCTET_STRING, false),
            Ty::Struct(..) => (false, TAG_SEQUENCE, true),
            Ty::SeqOf(_, set, _) => (false, if *set { TAG_SET } else { TAG_SEQUENCE }, true),
        }
    }
}

/// A field's struct tag.
#[derive(Debug, Clone, Copy, Default)]
struct P {
    optional: bool,
    explicit: bool,
    tag: Option<i64>,
    default: Option<i64>,
}

const NONE: P = P {
    optional: false,
    explicit: false,
    tag: None,
    default: None,
};

const OPT: P = P {
    optional: true,
    explicit: false,
    tag: None,
    default: None,
};

const fn tagged(tag: i64) -> P {
    P {
        optional: true,
        explicit: false,
        tag: Some(tag),
        default: None,
    }
}

const fn explicit(tag: i64) -> P {
    P {
        optional: true,
        explicit: true,
        tag: Some(tag),
        default: None,
    }
}

#[derive(Debug, Clone, Copy)]
struct Field {
    name: &'static str,
    ty: Ty,
    p: P,
}

const fn f(name: &'static str, ty: Ty, p: P) -> Field {
    Field { name, ty, p }
}

/// What a field was read as.
#[derive(Debug, Clone, PartialEq, Eq)]
enum V<'a> {
    Raw {
        class: u8,
        tag: i64,
        compound: bool,
        bytes: &'a [u8],
        full: &'a [u8],
    },
    Oid(Vec<i64>),
    Bits {
        bytes: &'a [u8],
        bit_length: usize,
    },
    Time(Time),
    Big(&'a [u8]),
    Int(i64),
    Bool(bool),
    Bytes(&'a [u8]),
    Any,
    Struct {
        full: &'a [u8],
        fields: Vec<Option<V<'a>>>,
    },
    List(Vec<V<'a>>),
}

fn check_integer(b: &[u8], lax: bool, name: &str) -> Result<(), String> {
    match b {
        [] => Err(st("empty integer", name)),
        [_] => Ok(()),
        _ if lax => Ok(()),
        [0, s, ..] if s & 0x80 == 0 => Err(st("integer not minimally-encoded", name)),
        [0xff, s, ..] if s & 0x80 == 0x80 => Err(st("integer not minimally-encoded", name)),
        _ => Ok(()),
    }
}

fn int64(b: &[u8], lax: bool, name: &str) -> Result<i64, String> {
    check_integer(b, lax, name)?;
    if b.len() > 8 {
        return Err(st("integer too large", name));
    }
    let mut v: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) {
        -1
    } else {
        0
    };
    for &x in b {
        v = (v << 8) | i64::from(x);
    }
    Ok(v)
}

fn bits<'a>(b: &'a [u8], name: &str) -> Result<V<'a>, String> {
    let Some((&pad, rest)) = b.split_first() else {
        return Err(syn("zero length BIT STRING", name));
    };
    let last = b.last().copied().unwrap_or(0);
    if pad > 7 || (b.len() == 1 && pad > 0) || last & ((1u8 << pad.min(7)) - 1) != 0 {
        return Err(syn("invalid padding bits in BIT STRING", name));
    }
    Ok(V::Bits {
        bytes: rest,
        bit_length: rest.len() * 8 - usize::from(pad),
    })
}

fn oid(b: &[u8], lax: bool, name: &str) -> Result<Vec<i64>, String> {
    if b.is_empty() {
        if lax {
            return Ok(Vec::new());
        }
        return Err(syn("zero length OBJECT IDENTIFIER", name));
    }
    let (v, mut offset) = base128(b, 0, name)?;
    let mut s = if v < 80 {
        vec![v / 40, v % 40]
    } else {
        vec![2, v - 80]
    };
    while offset < b.len() {
        let (v, o) = base128(b, offset, name)?;
        s.push(v);
        offset = o;
    }
    Ok(s)
}

fn printable(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || (b'\''..=b')').contains(&b)
        || (b'+'..=b'/').contains(&b)
        || matches!(b, b' ' | b':' | b'=' | b'?' | b'*' | b'&')
}

fn could_be_iso8859_1(b: &[u8]) -> bool {
    b.iter().all(|&x| !(x < 0x20 || (0x7f..0xa0).contains(&x)))
}

fn could_be_t61(b: &[u8]) -> bool {
    !b.iter().any(|x| {
        matches!(
            x,
            0x00 | 0x23
                | 0x24
                | 0x5c
                | 0x5e
                | 0x60
                | 0x7b
                | 0x7d
                | 0x7e
                | 0xa5
                | 0xa6
                | 0xac
                | 0xad
                | 0xae
                | 0xaf
                | 0xb9
                | 0xba
                | 0xc0
                | 0xc9
                | 0xd0..=0xdc | 0xde | 0xdf | 0xe5 | 0xff
        )
    })
}

/// parsePrintableString.
fn printable_string(b: &[u8], lax: bool, name: &str) -> Result<(), String> {
    if b.iter().all(|&x| printable(x)) {
        return Ok(());
    }
    if !lax {
        return Err(syn("PrintableString contains invalid character", name));
    }
    if could_be_iso8859_1(b) || could_be_t61(b) {
        return Ok(());
    }
    Err(syn(
        "PrintableString contains invalid character, couldn't determine correct String type",
        name,
    ))
}

/// parseUTCTime.
fn utc_time(b: &[u8]) -> Result<Time, String> {
    let layout = match gotime::parse("0601021504Z0700", b) {
        Ok(_) => "0601021504Z0700",
        Err(_) => "060102150405Z0700",
    };
    let t = gotime::parse_exact(layout, b)?;
    Ok(if gotime::year(&t) >= 2050 {
        gotime::add_years(&t, -100)
    } else {
        t
    })
}

/// parseGeneralizedTime (this fork's: no fraction).
fn generalized_time(b: &[u8]) -> Result<Time, String> {
    gotime::parse_exact("20060102150405Z0700", b)
}

/// fieldParameters printed by %+v, as a tag mismatch prints them.
fn params_text(p: &P, lax: bool, name: &str) -> String {
    let ptr = |o: Option<i64>| if o.is_some() { "(ptr)" } else { "<nil>" };
    format!(
        "{{optional:{} explicit:{} application:false private:false defaultValue:{} tag:{} stringType:0 timeType:0 set:false omitEmpty:false lax:{} name:{}}}",
        p.optional,
        p.explicit,
        ptr(p.default),
        ptr(p.tag),
        lax,
        name
    )
}

/// parseField: the value (None where an optional field is absent) and the offset after.
fn field<'a>(
    ty: &Ty,
    bytes: &'a [u8],
    init: usize,
    p: &P,
    name: &str,
    lax: bool,
) -> Result<(Option<V<'a>>, usize), String> {
    if init == bytes.len() {
        if p.optional {
            return Ok((None, init));
        }
        return Err(syn("sequence truncated", name));
    }
    if let Ty::Any = ty {
        let (t, offset) = tag_and_length(bytes, init, name)?;
        let end = offset
            .checked_add(t.length)
            .filter(|e| *e <= bytes.len())
            .ok_or_else(|| syn("data truncated", name))?;
        let inner = bytes.get(offset..end).unwrap_or_default();
        if !t.compound && t.class == UNIVERSAL {
            match t.tag {
                TAG_PRINTABLE => printable_string(inner, lax, name)?,
                TAG_NUMERIC => {
                    if !inner.iter().all(|x| x.is_ascii_digit() || *x == b' ') {
                        return Err(syn("NumericString contains invalid character", name));
                    }
                }
                TAG_IA5 => {
                    if !inner.is_ascii() {
                        return Err(syn("IA5String contains invalid character", name));
                    }
                }
                TAG_UTF8 => {
                    if std::str::from_utf8(inner).is_err() {
                        return Err("asn1: invalid UTF-8 string".into());
                    }
                }
                TAG_INTEGER => {
                    int64(inner, lax, name)?;
                }
                TAG_BIT_STRING => {
                    bits(inner, name)?;
                }
                TAG_OID => {
                    oid(inner, lax, name)?;
                }
                TAG_UTC_TIME => {
                    utc_time(inner)?;
                }
                TAG_GENERALIZED_TIME => {
                    generalized_time(inner)?;
                }
                TAG_BMP if !inner.len().is_multiple_of(2) => {
                    return Err("pkcs12: odd-length BMP string".into());
                }
                _ => {}
            }
        }
        return Ok((Some(V::Any), end));
    }
    let (mut t, mut offset) = tag_and_length(bytes, init, name)?;
    if p.explicit {
        if offset == bytes.len() {
            return Err(st("explicit tag has no child", name));
        }
        if t.class == CONTEXT && Some(t.tag) == p.tag && (t.length == 0 || t.compound) {
            if let Ty::Raw = ty {
            } else if t.length > 0 {
                let (inner, o) = tag_and_length(bytes, offset, name)?;
                t = inner;
                offset = o;
            } else {
                return Err(st("zero length explicit tag was not an asn1.Flag", name));
            }
        } else if p.optional {
            return Ok((None, init));
        } else {
            return Err(st("explicitly tagged member didn't match", name));
        }
    }
    let (match_any, mut universal, compound) = ty.universal();
    if universal == TAG_UTC_TIME && t.tag == TAG_GENERALIZED_TIME && t.class == UNIVERSAL {
        universal = TAG_GENERALIZED_TIME;
    }
    let mut match_any_class_and_tag = match_any;
    let mut expected_class = UNIVERSAL;
    let mut expected_tag = universal;
    if !p.explicit
        && let Some(tag) = p.tag
    {
        expected_class = CONTEXT;
        expected_tag = tag;
        match_any_class_and_tag = false;
    }
    if (!match_any_class_and_tag && (t.class != expected_class || t.tag != expected_tag))
        || (!match_any && t.compound != compound)
    {
        if p.optional {
            return Ok((None, init));
        }
        return Err(st(
            &format!(
                "tags don't match ({expected_tag} vs {{class:{} tag:{} length:{} isCompound:{}}}) {} {} @{offset}",
                t.class,
                t.tag,
                t.length,
                t.compound,
                params_text(p, lax, name),
                ty.go_name()
            ),
            name,
        ));
    }
    let end = offset
        .checked_add(t.length)
        .filter(|e| *e <= bytes.len())
        .ok_or_else(|| syn("data truncated", name))?;
    let inner = bytes.get(offset..end).unwrap_or_default();
    let full = bytes.get(init..end).unwrap_or_default();
    let v = match ty {
        Ty::Raw => V::Raw {
            class: t.class,
            tag: t.tag,
            compound: t.compound,
            bytes: inner,
            full,
        },
        Ty::Oid => V::Oid(oid(inner, lax, name)?),
        Ty::Bits => bits(inner, name)?,
        Ty::Time => V::Time(if universal == TAG_UTC_TIME {
            utc_time(inner)?
        } else {
            generalized_time(inner)?
        }),
        Ty::Big => {
            check_integer(inner, lax, name)?;
            V::Big(inner)
        }
        Ty::Int => V::Int(int64(inner, lax, name)?),
        Ty::Bool => V::Bool(match inner {
            [0] => false,
            [0xff] => true,
            _ => return Err(syn("invalid boolean", name)),
        }),
        Ty::Bytes => V::Bytes(inner),
        Ty::Struct(fields, _) => {
            let mut out = Vec::with_capacity(fields.len());
            let mut o = 0;
            for fl in *fields {
                let (v, next) = field(&fl.ty, inner, o, &fl.p, fl.name, lax)?;
                out.push(v);
                o = next;
            }
            V::Struct { full, fields: out }
        }
        Ty::SeqOf(elem, _, _) => V::List(sequence_of(elem, inner, lax, name)?),
        Ty::Any => V::Any,
    };
    Ok((Some(v), end))
}

/// parseSequenceOf.
fn sequence_of<'a>(elem: &Ty, bytes: &'a [u8], lax: bool, name: &str) -> Result<Vec<V<'a>>, String> {
    let (match_any, expected, compound) = elem.universal();
    let mut n = 0;
    let mut offset = 0;
    while offset < bytes.len() {
        let (mut t, o) = tag_and_length(bytes, offset, name)?;
        match t.tag {
            TAG_IA5 | TAG_GENERAL_STRING | TAG_T61 | TAG_UTF8 | TAG_NUMERIC | TAG_BMP => {
                t.tag = TAG_PRINTABLE
            }
            TAG_GENERALIZED_TIME | TAG_UTC_TIME => t.tag = TAG_UTC_TIME,
            _ => {}
        }
        if !match_any && (t.class != UNIVERSAL || t.compound != compound || t.tag != expected) {
            return Err(st(
                &format!(
                    "sequence tag mismatch (got:{{class:{} tag:{} length:{} isCompound:{}}}, want:0/{expected}/{compound})",
                    t.class, t.tag, t.length, t.compound
                ),
                name,
            ));
        }
        offset = o
            .checked_add(t.length)
            .filter(|e| *e <= bytes.len())
            .ok_or_else(|| syn("truncated sequence", name))?;
        n += 1;
    }
    let mut out = Vec::with_capacity(n);
    let mut offset = 0;
    for _ in 0..n {
        let (v, o) = field(elem, bytes, offset, &NONE, "", lax)?;
        if let Some(v) = v {
            out.push(v);
        }
        offset = o;
    }
    Ok(out)
}

/// Unmarshal: the value and the octets after it.
fn unmarshal<'a>(ty: &Ty, b: &'a [u8], lax: bool) -> Result<(V<'a>, &'a [u8]), String> {
    let (v, o) = field(ty, b, 0, &NONE, "", lax)?;
    let v = v.ok_or_else(|| syn("sequence truncated", ""))?;
    Ok((v, b.get(o..).unwrap_or_default()))
}

// ---------------------------------------------------------------------------------------
// The CT fork of crypto/x509's types.

const ALGORITHM_IDENTIFIER: [Field; 2] = [f("Algorithm", Ty::Oid, NONE), f("Parameters", Ty::Raw, OPT)];
const AI: Ty = Ty::Struct(&ALGORITHM_IDENTIFIER, "AlgorithmIdentifier");
const VALIDITY: [Field; 2] = [f("NotBefore", Ty::Time, NONE), f("NotAfter", Ty::Time, NONE)];
const PUBLIC_KEY_INFO: [Field; 2] = [f("Algorithm", AI, NONE), f("PublicKey", Ty::Bits, NONE)];
const EXTENSION: [Field; 3] = [
    f("Id", Ty::Oid, NONE),
    f("Critical", Ty::Bool, OPT),
    f("Value", Ty::Bytes, NONE),
];
const EXT: Ty = Ty::Struct(&EXTENSION, "Extension");
const TBS: [Field; 10] = [
    f(
        "Version",
        Ty::Int,
        P {
            optional: true,
            explicit: true,
            tag: Some(0),
            default: Some(0),
        },
    ),
    f("SerialNumber", Ty::Big, NONE),
    f("SignatureAlgorithm", AI, NONE),
    f("Issuer", Ty::Raw, NONE),
    f("Validity", Ty::Struct(&VALIDITY, "validity"), NONE),
    f("Subject", Ty::Raw, NONE),
    f("PublicKey", Ty::Struct(&PUBLIC_KEY_INFO, "publicKeyInfo"), NONE),
    f("UniqueId", Ty::Bits, tagged(1)),
    f("SubjectUniqueId", Ty::Bits, tagged(2)),
    f("Extensions", Ty::SeqOf(&EXT, false, ""), explicit(3)),
];
const TBS_TY: Ty = Ty::Struct(&TBS, "tbsCertificate");
const CERTIFICATE: [Field; 3] = [
    f("TBSCertificate", TBS_TY, NONE),
    f("SignatureAlgorithm", AI, NONE),
    f("SignatureValue", Ty::Bits, NONE),
];
const CERT_TY: Ty = Ty::Struct(&CERTIFICATE, "certificate");

const ATTRIBUTE: [Field; 2] = [f("Type", Ty::Oid, NONE), f("Value", Ty::Any, NONE)];
const ATV: Ty = Ty::Struct(&ATTRIBUTE, "AttributeTypeAndValue");
const RDN_SET: Ty = Ty::SeqOf(&ATV, true, "RelativeDistinguishedNameSET");
const RDN_SEQUENCE: Ty = Ty::SeqOf(&RDN_SET, false, "RDNSequence");

const PKCS1: [Field; 2] = [f("N", Ty::Big, NONE), f("E", Ty::Int, NONE)];
const DSA_PARAMS: [Field; 3] = [
    f("P", Ty::Big, NONE),
    f("Q", Ty::Big, NONE),
    f("G", Ty::Big, NONE),
];
const BASIC_CONSTRAINTS: [Field; 2] = [
    f("IsCA", Ty::Bool, OPT),
    f(
        "MaxPathLen",
        Ty::Int,
        P {
            optional: true,
            explicit: false,
            tag: None,
            default: Some(-1),
        },
    ),
];
const AUTH_KEY_ID: [Field; 1] = [f("Id", Ty::Bytes, tagged(0))];
const DP_NAME: [Field; 2] = [
    f("FullName", Ty::SeqOf(&Ty::Raw, false, ""), tagged(0)),
    f("RelativeName", RDN_SEQUENCE, tagged(1)),
];
const DISTRIBUTION_POINT: [Field; 3] = [
    f(
        "DistributionPoint",
        Ty::Struct(&DP_NAME, "distributionPointName"),
        tagged(0),
    ),
    f("Reason", Ty::Bits, tagged(1)),
    f("CRLIssuer", Ty::Raw, tagged(2)),
];
const DP: Ty = Ty::Struct(&DISTRIBUTION_POINT, "distributionPoint");
const POLICY_INFORMATION: [Field; 1] = [f("Policy", Ty::Oid, NONE)];
const POLICY: Ty = Ty::Struct(&POLICY_INFORMATION, "policyInformation");
const ACCESS_DESCRIPTION: [Field; 2] = [f("Method", Ty::Oid, NONE), f("Location", Ty::Raw, NONE)];
const ACCESS: Ty = Ty::Struct(&ACCESS_DESCRIPTION, "accessDescription");
const IP_ADDRESS_FAMILY: [Field; 2] = [f("AddressFamily", Ty::Bytes, NONE), f("Choice", Ty::Raw, NONE)];
const IP_FAMILY: Ty = Ty::Struct(&IP_ADDRESS_FAMILY, "ipAddressFamily");
const IP_ADDRESS_RANGE: [Field; 2] = [f("Min", Ty::Bits, NONE), f("Max", Ty::Bits, NONE)];
const AS_IDENTIFIERS: [Field; 2] = [f("ASNum", Ty::Raw, tagged(0)), f("RDI", Ty::Raw, tagged(1))];
const ASID_RANGE: [Field; 2] = [f("Min", Ty::Int, NONE), f("Max", Ty::Int, NONE)];

fn arcs(oid: &[i64], want: &[i64]) -> bool {
    oid == want
}

const OID_RSA: [i64; 7] = [1, 2, 840, 113549, 1, 1, 1];
const OID_RSAES_OAEP: [i64; 7] = [1, 2, 840, 113549, 1, 1, 7];
const OID_DSA: [i64; 6] = [1, 2, 840, 10040, 4, 1];
const OID_ECDSA: [i64; 6] = [1, 2, 840, 10045, 2, 1];
const OID_ED25519: [i64; 4] = [1, 3, 101, 112];
const OID_AIA: [i64; 9] = [1, 3, 6, 1, 5, 5, 7, 1, 1];
const OID_SIA: [i64; 9] = [1, 3, 6, 1, 5, 5, 7, 1, 11];
const OID_IP_PREFIX_LIST: [i64; 9] = [1, 3, 6, 1, 5, 5, 7, 1, 7];
const OID_AS_LIST: [i64; 9] = [1, 3, 6, 1, 5, 5, 7, 1, 8];
const OID_CT_SCT: [i64; 10] = [1, 3, 6, 1, 4, 1, 11129, 2, 4, 2];

/// An extension: its OID's arcs, critical, its value.
type Ext<'a> = (Vec<i64>, bool, &'a [u8]);

/// A TBSCertificate as this fork reads it.
#[derive(Debug, Clone)]
struct Tbs<'a> {
    version: i64,
    serial: &'a [u8],
    sig_alg: (Vec<i64>, Option<&'a [u8]>),
    sig_alg_full: &'a [u8],
    issuer: &'a [u8],
    not_before: Time,
    not_after: Time,
    subject: &'a [u8],
    spki: &'a [u8],
    key_alg: (Vec<i64>, Option<&'a [u8]>),
    key: (&'a [u8], usize),
    unique_id: Option<(&'a [u8], usize)>,
    subject_unique_id: Option<(&'a [u8], usize)>,
    /// None where the [3] is absent.
    extensions: Option<Vec<Ext<'a>>>,
}

fn ai_of<'a>(v: Option<&V<'a>>) -> (Vec<i64>, Option<&'a [u8]>, &'a [u8]) {
    match v {
        Some(V::Struct { full, fields }) => {
            let oid = match fields.first() {
                Some(Some(V::Oid(o))) => o.clone(),
                _ => Vec::new(),
            };
            let params = match fields.get(1) {
                Some(Some(V::Raw { full, .. })) => Some(*full),
                _ => None,
            };
            (oid, params, full)
        }
        _ => (Vec::new(), None, &[]),
    }
}

fn bits_of<'a>(v: Option<&Option<V<'a>>>) -> Option<(&'a [u8], usize)> {
    match v {
        Some(Some(V::Bits { bytes, bit_length })) => Some((bytes, *bit_length)),
        _ => None,
    }
}

fn tbs_of<'a>(v: &V<'a>) -> Option<Tbs<'a>> {
    let V::Struct { fields, .. } = v else {
        return None;
    };
    let get = |i: usize| fields.get(i).and_then(Option::as_ref);
    let version = match get(0) {
        Some(V::Int(v)) => *v,
        _ => 0,
    };
    let serial = match get(1) {
        Some(V::Big(b)) => *b,
        _ => return None,
    };
    let (sa, sp, sfull) = ai_of(get(2));
    let issuer = match get(3) {
        Some(V::Raw { full, .. }) => *full,
        _ => return None,
    };
    let (not_before, not_after) = match get(4) {
        Some(V::Struct { fields, .. }) => match (fields.first(), fields.get(1)) {
            (Some(Some(V::Time(a))), Some(Some(V::Time(b)))) => (*a, *b),
            _ => return None,
        },
        _ => return None,
    };
    let subject = match get(5) {
        Some(V::Raw { full, .. }) => *full,
        _ => return None,
    };
    let (spki, key_alg, key) = match get(6) {
        Some(V::Struct { full, fields }) => {
            let (ka, kp, _) = ai_of(fields.first().and_then(Option::as_ref));
            (*full, (ka, kp), bits_of(fields.get(1))?)
        }
        _ => return None,
    };
    let extensions = match get(9) {
        Some(V::List(list)) => Some(
            list.iter()
                .filter_map(|e| match e {
                    V::Struct { fields, .. } => {
                        let id = match fields.first() {
                            Some(Some(V::Oid(o))) => o.clone(),
                            _ => return None,
                        };
                        let critical = matches!(fields.get(1), Some(Some(V::Bool(true))));
                        let value = match fields.get(2) {
                            Some(Some(V::Bytes(b))) => *b,
                            _ => return None,
                        };
                        Some((id, critical, value))
                    }
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    };
    Some(Tbs {
        version,
        serial,
        sig_alg: (sa, sp),
        sig_alg_full: sfull,
        issuer,
        not_before,
        not_after,
        subject,
        spki,
        key_alg,
        key,
        unique_id: bits_of(fields.get(7)),
        subject_unique_id: bits_of(fields.get(8)),
        extensions,
    })
}

/// What `ParseCertificate` makes of a certificate: a fatal error, or its non-fatal
/// errors and its SCT list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CtParse {
    fatal: Option<String>,
    nonfatal: Vec<String>,
    sct_list: Vec<Vec<u8>>,
}

impl CtParse {
    /// The error ParseCertificate returns: the fatal one, or NonFatalErrors.
    fn error(&self) -> Option<String> {
        if let Some(f) = &self.fatal {
            return Some(f.clone());
        }
        if self.nonfatal.is_empty() {
            return None;
        }
        let mut s = String::from("NonFatalErrors: ");
        for e in &self.nonfatal {
            s.push_str(e);
            s.push_str("; ");
        }
        Some(s)
    }
}

/// domainToReverseLabels' verdict, as this fork has it.
fn reverse_labels_ok(domain: &[u8]) -> bool {
    let mut labels: Vec<&[u8]> = Vec::new();
    let mut d = domain;
    while !d.is_empty() {
        match d.iter().rposition(|c| *c == b'.') {
            None => {
                labels.push(d);
                d = &[];
            }
            Some(i) => {
                labels.push(d.get(i + 1..).unwrap_or_default());
                d = d.get(..i).unwrap_or_default();
            }
        }
    }
    if labels.first().is_some_and(|l| l.is_empty()) {
        return false;
    }
    labels.iter().all(|l| {
        !l.is_empty()
            && String::from_utf8_lossy(l)
                .chars()
                .all(|c| (33..=126).contains(&(c as u32)))
    })
}

/// The fatal error, if any, of an extension value whose trailing data is refused.
fn whole<'a>(ty: &Ty, value: &'a [u8], trailing: &str) -> Result<V<'a>, String> {
    let (v, rest) = unmarshal(ty, value, false)?;
    if !rest.is_empty() {
        return Err(trailing.to_string());
    }
    Ok(v)
}

/// parseRPKIAddrBlocks' non-fatal errors.
fn rpki_addr_blocks(data: &[u8], nfe: &mut Vec<String>) {
    let list = Ty::SeqOf(&IP_FAMILY, false, "");
    let blocks = match unmarshal(&list, data, false) {
        Err(e) => {
            nfe.push(format!("failed to asn1.Unmarshal ipAddrBlocks extension: {e}"));
            return;
        }
        Ok((_, rest)) if !rest.is_empty() => {
            nfe.push("trailing data after ipAddrBlocks extension".into());
            return;
        }
        Ok((V::List(l), _)) => l,
        Ok(_) => Vec::new(),
    };
    for (i, block) in blocks.iter().enumerate() {
        let V::Struct { fields, .. } = block else {
            continue;
        };
        let family = match fields.first() {
            Some(Some(V::Bytes(b))) => *b,
            _ => &[],
        };
        if !(2..=3).contains(&family.len()) {
            nfe.push(format!(
                "invalid address family length ({}) for ipAddrBlock.addressFamily",
                family.len()
            ));
            continue;
        }
        let choice = match fields.get(1) {
            Some(Some(V::Raw { full, .. })) => *full,
            _ => &[],
        };
        if choice == [0x05, 0x00] {
            continue;
        }
        let ranges = match unmarshal(&Ty::SeqOf(&Ty::Raw, false, ""), choice, false) {
            Ok((V::List(l), _)) => l,
            Ok(_) => Vec::new(),
            Err(e) => {
                nfe.push(format!(
                    "failed to asn1.Unmarshal ipAddrBlocks[{i}].ipAddressChoice.addressesOrRanges: {e}"
                ));
                continue;
            }
        };
        for (j, ar) in ranges.iter().enumerate() {
            let V::Raw {
                class,
                tag,
                compound,
                bytes,
                full,
            } = ar
            else {
                continue;
            };
            match *tag {
                TAG_BIT_STRING => {
                    if let Err(e) = unmarshal(&Ty::Bits, full, false) {
                        nfe.push(format!(
                            "failed to asn1.Unmarshal ipAddrBlocks[{i}].ipAddressChoice.addressesOrRanges[{j}].addressPrefix: {e}"
                        ));
                    }
                }
                TAG_SEQUENCE => {
                    if let Err(e) = unmarshal(&Ty::Struct(&IP_ADDRESS_RANGE, "ipAddressRange"), full, false) {
                        nfe.push(format!(
                            "failed to asn1.Unmarshal ipAddrBlocks[{i}].ipAddressChoice.addressesOrRanges[{j}].addressRange: {e}"
                        ));
                    }
                }
                _ => nfe.push(format!(
                    "unexpected ASN.1 type in ipAddrBlocks[{i}].ipAddressChoice.addressesOrRanges[{j}]: {}",
                    raw_text(*class, *tag, *compound, bytes, full)
                )),
            }
        }
    }
}

/// %+v of an asn1.RawValue.
fn raw_text(class: u8, tag: i64, compound: bool, bytes: &[u8], full: &[u8]) -> String {
    let list = |b: &[u8]| b.iter().map(u8::to_string).collect::<Vec<_>>().join(" ");
    format!(
        "{{Class:{class} Tag:{tag} IsCompound:{compound} Bytes:[{}] FullBytes:[{}]}}",
        list(bytes),
        list(full)
    )
}

/// parseASIDChoice's non-fatal errors.
fn asid_choice(v: Option<&V<'_>>, nfe: &mut Vec<String>) {
    let Some(V::Raw { bytes, full, .. }) = v else {
        return;
    };
    if full.is_empty() || *bytes == [0x05, 0x00] {
        return;
    }
    let ids = match unmarshal(&Ty::SeqOf(&Ty::Raw, false, ""), bytes, false) {
        Err(e) => {
            nfe.push(format!(
                "failed to asn1.Unmarshal ASIdentifiers.asIdsOrRanges: {e}"
            ));
            return;
        }
        Ok((_, rest)) if !rest.is_empty() => {
            nfe.push("trailing data after ASIdentifiers.asIdsOrRanges".into());
            return;
        }
        Ok((V::List(l), _)) => l,
        Ok(_) => Vec::new(),
    };
    for (i, id) in ids.iter().enumerate() {
        let V::Raw {
            class,
            tag,
            compound,
            bytes,
            full,
        } = id
        else {
            continue;
        };
        match *tag {
            TAG_INTEGER => {
                if let Err(e) = unmarshal(&Ty::Int, full, false) {
                    nfe.push(format!(
                        "failed to asn1.Unmarshal ASIdentifiers.asIdsOrRanges[{i}].id: {e}"
                    ));
                }
            }
            TAG_SEQUENCE => {
                if let Err(e) = unmarshal(&Ty::Struct(&ASID_RANGE, "ASIDRange"), full, false) {
                    nfe.push(format!(
                        "failed to asn1.Unmarshal ASIdentifiers.asIdsOrRanges[{i}].range: {e}"
                    ));
                }
            }
            _ => nfe.push(format!(
                "unexpected value in ASIdentifiers.asIdsOrRanges[{i}]: {}",
                raw_text(*class, *tag, *compound, bytes, full)
            )),
        }
    }
}

/// parseRPKIASIdentifiers' non-fatal errors.
fn rpki_as_identifiers(data: &[u8], nfe: &mut Vec<String>) {
    match unmarshal(&Ty::Struct(&AS_IDENTIFIERS, "asIdentifiers"), data, false) {
        Err(e) => nfe.push(format!("failed to asn1.Unmarshal ASIdentifiers extension: {e}")),
        Ok((_, rest)) if !rest.is_empty() => nfe.push("trailing data after ASIdentifiers extension".into()),
        Ok((V::Struct { fields, .. }, _)) => {
            asid_choice(fields.first().and_then(Option::as_ref), nfe);
            asid_choice(fields.get(1).and_then(Option::as_ref), nfe);
        }
        Ok(_) => {}
    }
}

/// parsePublicKey's errors (fatal) and non-fatal errors, for the algorithms this fork
/// knows.
fn public_key(tbs: &Tbs<'_>, nfe: &mut Vec<String>) -> Result<(), String> {
    let (alg, params) = (&tbs.key_alg.0, tbs.key_alg.1);
    let data = crate::asn1::right_align(tbs.key.0, tbs.key.1);
    if arcs(alg, &OID_RSA) {
        if params != Some(&[0x05, 0x00][..]) {
            nfe.push("x509: RSA key missing NULL parameters".into());
        }
        let ty = Ty::Struct(&PKCS1, "pkcs1PublicKey");
        let (v, rest) = match unmarshal(&ty, &data, false) {
            Ok(r) => r,
            Err(e) => {
                let lax = unmarshal(&ty, &data, true)?;
                nfe.push(e);
                lax
            }
        };
        if !rest.is_empty() {
            return Err("x509: trailing data after RSA public key".into());
        }
        if let V::Struct { fields, .. } = v {
            if let Some(Some(V::Big(n))) = fields.first()
                && (n.first().is_some_and(|b| b & 0x80 != 0) || n.iter().all(|b| *b == 0))
            {
                nfe.push("x509: RSA modulus is not a positive number".into());
            }
            if let Some(Some(V::Int(e))) = fields.get(1)
                && *e <= 0
            {
                return Err("x509: RSA public exponent is not a positive number".into());
            }
        }
    } else if arcs(alg, &OID_DSA) {
        let (_, rest) = match unmarshal(&Ty::Big, &data, false) {
            Ok(r) => r,
            Err(e) => {
                let lax = unmarshal(&Ty::Big, &data, true)?;
                nfe.push(e);
                lax
            }
        };
        if !rest.is_empty() {
            return Err("x509: trailing data after DSA public key".into());
        }
        let (_, rest) = unmarshal(
            &Ty::Struct(&DSA_PARAMS, "dsaAlgorithmParameters"),
            params.unwrap_or_default(),
            false,
        )?;
        if !rest.is_empty() {
            return Err("x509: trailing data after DSA parameters".into());
        }
    }
    // ECDSA and Ed25519 keys Go's crypto/x509 accepted pass here unchanged: their
    // parameters are one element, and the point was already found on its curve.
    let _ = (&OID_ECDSA, &OID_ED25519, &OID_RSAES_OAEP);
    Ok(())
}

/// The names this fork reads: the strict error a lax reading forgave, or a fatal one.
fn name(full: &[u8], nfe: &mut Vec<String>) -> Result<(), String> {
    match unmarshal(&RDN_SEQUENCE, full, false) {
        Ok((_, rest)) if !rest.is_empty() => Err("x509: trailing data after X.509 subject".into()),
        Ok(_) => Ok(()),
        Err(e) => {
            unmarshal(&RDN_SEQUENCE, full, true)?;
            nfe.push(e);
            Ok(())
        }
    }
}

/// One extension, as parseCertificate's loop handles it.
fn extension(id: &[i64], value: &[u8], nfe: &mut Vec<String>, scts: &mut Vec<Vec<u8>>) -> Result<(), String> {
    if let [2, 5, 29, n] = id {
        match n {
            15 => {
                whole(&Ty::Bits, value, "x509: trailing data after X.509 KeyUsage")?;
            }
            19 => {
                whole(
                    &Ty::Struct(&BASIC_CONSTRAINTS, "basicConstraints"),
                    value,
                    "x509: trailing data after X.509 BasicConstraints",
                )?;
            }
            17 => san(value, nfe)?,
            30 => name_constraints(value, nfe)?,
            31 => {
                whole(
                    &Ty::SeqOf(&DP, false, ""),
                    value,
                    "x509: trailing data after X.509 CRL distribution point",
                )?;
            }
            35 => {
                whole(
                    &Ty::Struct(&AUTH_KEY_ID, "authKeyId"),
                    value,
                    "x509: trailing data after X.509 authority key-id",
                )?;
            }
            37 => {
                if value.is_empty() {
                    nfe.push("x509: empty ExtendedKeyUsage".into());
                } else {
                    let ty = Ty::SeqOf(&Ty::Oid, false, "");
                    let rest = match unmarshal(&ty, value, false) {
                        Ok((_, rest)) => rest,
                        Err(e) => {
                            let (_, rest) = unmarshal(&ty, value, true)?;
                            nfe.push(e);
                            rest
                        }
                    };
                    if !rest.is_empty() {
                        return Err("x509: trailing data after X.509 ExtendedKeyUsage".into());
                    }
                }
            }
            14 => {
                whole(&Ty::Bytes, value, "x509: trailing data after X.509 key-id")?;
            }
            32 => {
                whole(
                    &Ty::SeqOf(&POLICY, false, ""),
                    value,
                    "x509: trailing data after X.509 certificate policies",
                )?;
            }
            _ => {}
        }
        return Ok(());
    }
    if arcs(id, &OID_AIA) || arcs(id, &OID_SIA) {
        let aia = arcs(id, &OID_AIA);
        let v = whole(
            &Ty::SeqOf(&ACCESS, false, ""),
            value,
            if aia {
                "x509: trailing data after X.509 authority information"
            } else {
                "x509: trailing data after X.509 subject information"
            },
        )?;
        if matches!(&v, V::List(l) if l.is_empty()) {
            nfe.push(if aia {
                "x509: empty AuthorityInfoAccess extension".into()
            } else {
                "x509: empty SubjectInfoAccess extension".into()
            });
        }
    } else if arcs(id, &OID_IP_PREFIX_LIST) {
        rpki_addr_blocks(value, nfe);
    } else if arcs(id, &OID_AS_LIST) {
        rpki_as_identifiers(value, nfe);
    } else if arcs(id, &OID_CT_SCT) {
        match unmarshal(&Ty::Bytes, value, false) {
            Err(e) => nfe.push(format!("failed to asn1.Unmarshal SCT list extension: {e}")),
            Ok((_, rest)) if !rest.is_empty() => nfe.push("trailing data after ASN1-encoded SCT list".into()),
            Ok((v, _)) => {
                let raw = match v {
                    V::Bytes(b) => b,
                    _ => &[],
                };
                match sct_list(raw) {
                    Err(e) => nfe.push(format!("failed to tls.Unmarshal SCT list: {e}")),
                    Ok((_, rest)) if !rest.is_empty() => {
                        nfe.push("trailing data after TLS-encoded SCT list".into())
                    }
                    Ok((list, _)) => *scts = list,
                }
            }
        }
    }
    Ok(())
}

/// This fork's parseRFC2821Mailbox: Go's local part, its own domainToReverseLabels.
fn mailbox_ok(input: &[u8]) -> bool {
    let Some(&first) = input.first() else {
        return false;
    };
    let mut rest = input;
    if first == b'"' {
        rest = rest.get(1..).unwrap_or_default();
        loop {
            let Some((&c, tail)) = rest.split_first() else {
                return false;
            };
            rest = tail;
            match c {
                b'"' => break,
                b'\\' => match rest.split_first() {
                    Some((&n, tail))
                        if n == 11 || n == 12 || (1..=9).contains(&n) || (14..=127).contains(&n) =>
                    {
                        rest = tail;
                    }
                    _ => return false,
                },
                11 | 12 | 32 | 33 | 127 => {}
                c if (1..=8).contains(&c)
                    || (14..=31).contains(&c)
                    || (35..=91).contains(&c)
                    || (93..=126).contains(&c) => {}
                _ => return false,
            }
        }
    } else {
        let mut local = Vec::new();
        while let Some(&c) = rest.first() {
            let atext = |c: u8| c.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&c);
            if c == b'\\' {
                rest = rest.get(1..).unwrap_or_default();
                let Some(&n) = rest.first() else {
                    return false;
                };
                local.push(n);
                rest = rest.get(1..).unwrap_or_default();
            } else if atext(c) {
                local.push(c);
                rest = rest.get(1..).unwrap_or_default();
            } else {
                break;
            }
        }
        if local.is_empty()
            || local.first() == Some(&b'.')
            || local.last() == Some(&b'.')
            || local.windows(2).any(|w| w == b"..")
        {
            return false;
        }
    }
    match rest.split_first() {
        Some((b'@', domain)) => reverse_labels_ok(domain),
        _ => false,
    }
}

/// This fork's parseNameConstraintsExtension: a domain it cannot read is a non-fatal
/// error here where Go's parser refuses the certificate.
fn name_constraints(value: &[u8], nfe: &mut Vec<String>) -> Result<(), String> {
    use crate::der::{self, Der};
    let invalid = || "x509: invalid NameConstraints extension".to_string();
    let mut outer = Der(value);
    let mut top = outer.read(der::SEQUENCE).ok_or_else(invalid)?;
    if !outer.is_empty() {
        return Err(invalid());
    }
    let permitted = top.optional(der::explicit(0)).ok_or_else(invalid)?;
    let excluded = top.optional(der::explicit(1)).ok_or_else(invalid)?;
    if !top.is_empty() {
        return Err(invalid());
    }
    if permitted.as_ref().is_none_or(Der::is_empty) && excluded.as_ref().is_none_or(Der::is_empty) {
        return Err("x509: empty name constraints extension".into());
    }
    let ia5 = |v: &[u8]| -> Result<(), String> {
        if v.is_ascii() {
            Ok(())
        } else {
            Err(format!(
                "x509: invalid constraint value: x509: {} cannot be encoded as an IA5String",
                shards_dockerfile::go::quote(v)
            ))
        }
    };
    let trim = |v: &'_ [u8]| -> Vec<u8> { v.strip_prefix(b".").unwrap_or(v).to_vec() };
    for mut list in [permitted, excluded].into_iter().flatten() {
        while !list.is_empty() {
            let mut seq = list.read(der::SEQUENCE).ok_or_else(invalid)?;
            let (tag, _, v) = seq.any_element().ok_or_else(invalid)?;
            let q = || shards_dockerfile::go::quote(v);
            match tag {
                0x82 => {
                    ia5(v)?;
                    if !reverse_labels_ok(&trim(v)) {
                        nfe.push(format!("x509: failed to parse dnsName constraint {}", q()));
                    }
                }
                0x87 => {
                    let mask = match v.len() {
                        8 => v.get(4..),
                        32 => v.get(16..),
                        l => return Err(format!("x509: IP constraint contained value of length {l}")),
                    }
                    .unwrap_or_default();
                    let mut seen_zero = false;
                    let valid = mask.iter().all(|&b| {
                        if seen_zero {
                            return b == 0;
                        }
                        match b {
                            0x00 | 0x80 | 0xc0 | 0xe0 | 0xf0 | 0xf8 | 0xfc | 0xfe => {
                                seen_zero = true;
                                true
                            }
                            0xff => true,
                            _ => false,
                        }
                    });
                    if !valid {
                        let hex: String = mask.iter().map(|b| format!("{b:02x}")).collect();
                        return Err(format!("x509: IP constraint contained invalid mask {hex}"));
                    }
                }
                0x81 => {
                    ia5(v)?;
                    let ok = if v.contains(&b'@') {
                        mailbox_ok(v)
                    } else {
                        reverse_labels_ok(&trim(v))
                    };
                    if !ok {
                        nfe.push(format!("x509: failed to parse rfc822Name constraint {}", q()));
                    }
                }
                0x86 => {
                    ia5(v)?;
                    if crate::x509_constraints::is_ip(v) {
                        return Err(format!(
                            "x509: failed to parse URI constraint {}: cannot be IP address",
                            q()
                        ));
                    }
                    if !reverse_labels_ok(&trim(v)) {
                        nfe.push(format!("x509: failed to parse URI constraint {}", q()));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// parseSANExtension (forEachSAN): any element's tag number, whatever its class.
fn san(value: &[u8], nfe: &mut Vec<String>) -> Result<(), String> {
    let (seq, rest) = unmarshal(&Ty::Raw, value, false)?;
    if !rest.is_empty() {
        return Err("x509: trailing data after X.509 extension".into());
    }
    let V::Raw {
        class,
        tag,
        compound,
        bytes,
        ..
    } = seq
    else {
        return Ok(());
    };
    if !compound || tag != TAG_SEQUENCE || class != UNIVERSAL {
        return Err(st("bad SAN sequence", ""));
    }
    let mut rest = bytes;
    while !rest.is_empty() {
        let (v, r) = unmarshal(&Ty::Raw, rest, false)?;
        rest = r;
        let V::Raw { tag, bytes: data, .. } = v else {
            continue;
        };
        match tag {
            6 => {
                let parsed = shards_dockerfile::url::parse(data).map_err(|e| {
                    format!(
                        "x509: cannot parse URI {}: {}",
                        shards_dockerfile::go::quote(data),
                        String::from_utf8_lossy(&e)
                    )
                })?;
                if !parsed.host.is_empty() && !reverse_labels_ok(&parsed.host) {
                    return Err(format!(
                        "x509: cannot parse URI {}: invalid domain",
                        shards_dockerfile::go::quote(data)
                    ));
                }
            }
            7 if data.len() != 4 && data.len() != 16 => {
                nfe.push(format!("x509: cannot parse IP address of length {}", data.len()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// ParseCertificate of the CT fork, for a certificate Go's crypto/x509 accepted.
fn ct_parse(der: &[u8]) -> CtParse {
    let mut out = CtParse::default();
    let mut nfe = Vec::new();
    let cert = match unmarshal(&CERT_TY, der, false) {
        Ok((v, rest)) => {
            if !rest.is_empty() {
                out.fatal = Some(syn("trailing data", ""));
                return out;
            }
            v
        }
        Err(e) => match unmarshal(&CERT_TY, der, true) {
            Ok((v, rest)) => {
                nfe.push(e);
                if !rest.is_empty() {
                    out.fatal = Some(syn("trailing data", ""));
                    return out;
                }
                v
            }
            Err(lax) => {
                out.fatal = Some(lax);
                return out;
            }
        },
    };
    let V::Struct { fields, .. } = &cert else {
        out.fatal = Some(syn("sequence truncated", ""));
        return out;
    };
    let (Some(Some(tbs_v)), outer) = (fields.first(), fields.get(1)) else {
        out.fatal = Some(syn("sequence truncated", ""));
        return out;
    };
    let Some(tbs) = tbs_of(tbs_v) else {
        out.fatal = Some(syn("sequence truncated", ""));
        return out;
    };
    let (_, _, outer_full) = ai_of(outer.and_then(Option::as_ref));
    let result = (|| -> Result<(), String> {
        if outer_full != tbs.sig_alg_full {
            return Err("x509: mismatching signature algorithm identifiers".into());
        }
        public_key(&tbs, &mut nfe)?;
        name(tbs.subject, &mut nfe)?;
        name(tbs.issuer, &mut nfe)?;
        for (id, _, value) in tbs.extensions.iter().flatten() {
            extension(id, value, &mut nfe, &mut out.sct_list)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        out.fatal = Some(e);
        out.sct_list.clear();
        return out;
    }
    out.nonfatal = nfe;
    out
}

// ---------------------------------------------------------------------------------------
// certificate-transparency-go/tls: the SCT list and its SCTs.

fn tls_syntax(name: &str, msg: &str) -> String {
    if name.is_empty() {
        format!("tls: syntax error: {msg}")
    } else {
        format!("tls: syntax error: {name}: {msg}")
    }
}

fn tls_structure(name: &str, msg: &str) -> String {
    if name.is_empty() {
        format!("tls: structure error: {msg}")
    } else {
        format!("tls: structure error: {name}: {msg}")
    }
}

/// readVarUint of `count` octets, with a slice's length bounds.
fn var_uint(
    data: &[u8],
    offset: usize,
    count: usize,
    bounds: Option<(u64, u64)>,
    name: &str,
) -> Result<u64, String> {
    let bytes = data
        .get(offset..)
        .and_then(|r| r.get(..count))
        .ok_or_else(|| tls_syntax(name, "truncated variable-length integer"))?;
    let v = bytes.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x));
    if let Some((min, max)) = bounds {
        if v < min {
            return Err(tls_structure(
                name,
                &format!("value {v} too small for minimum {min}"),
            ));
        }
        if v > max {
            return Err(tls_structure(
                name,
                &format!("value {v} too large for maximum {max}"),
            ));
        }
    }
    Ok(v)
}

/// A variable-length vector of octets: (contents, offset after).
fn tls_bytes<'a>(
    data: &'a [u8],
    offset: usize,
    count: usize,
    bounds: (u64, u64),
    name: &str,
) -> Result<(&'a [u8], usize), String> {
    let len = var_uint(data, offset, count, Some(bounds), name)?;
    let start = offset + count;
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let end = start
        .checked_add(len)
        .filter(|e| *e <= data.len())
        .ok_or_else(|| tls_syntax(name, "truncated slice"))?;
    Ok((data.get(start..end).unwrap_or_default(), end))
}

/// tls.Unmarshal of a SignedCertificateTimestampList: each SerializedSCT's Val, and the
/// octets after the list.
fn sct_list(data: &[u8]) -> Result<(Vec<Vec<u8>>, &[u8]), String> {
    let (inner, end) = tls_bytes(data, 0, 2, (1, 65335), "SCTList")?;
    let mut out = Vec::new();
    let mut o = 0;
    while o < inner.len() {
        let (val, next) = tls_bytes(inner, o, 2, (1, 65535), "Val")?;
        out.push(val.to_vec());
        o = next;
    }
    Ok((out, data.get(end..).unwrap_or_default()))
}

/// A SignedCertificateTimestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sct {
    version: u64,
    log_id: [u8; 32],
    timestamp: u64,
    extensions: Vec<u8>,
    hash: u64,
    signature_alg: u64,
    signature: Vec<u8>,
}

fn tls_fixed<'a>(
    data: &'a [u8],
    offset: usize,
    n: usize,
    name: &str,
    what: &str,
) -> Result<&'a [u8], String> {
    data.get(offset..)
        .and_then(|r| r.get(..n))
        .ok_or_else(|| tls_syntax(name, what))
}

/// ExtractSCT: tls.Unmarshal of a SignedCertificateTimestamp, nothing after it.
fn extract_sct(val: &[u8]) -> Result<Sct, String> {
    let parse = || -> Result<(Sct, usize), String> {
        let version = var_uint(val, 0, 1, None, "SCTVersion")?;
        let id = tls_fixed(val, 1, 32, "KeyID", "truncated array")?;
        let mut log_id = [0u8; 32];
        log_id.copy_from_slice(id);
        let ts = tls_fixed(val, 33, 8, "Timestamp", "truncated uint64")?;
        let timestamp = ts.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x));
        let (ext, o) = tls_bytes(val, 41, 2, (0, 65535), "Extensions")?;
        let hash = var_uint(val, o, 1, None, "Hash")?;
        let signature_alg = var_uint(val, o + 1, 1, None, "Signature")?;
        let (sig, end) = tls_bytes(val, o + 2, 2, (0, 65535), "Signature")?;
        Ok((
            Sct {
                version,
                log_id,
                timestamp,
                extensions: ext.to_vec(),
                hash,
                signature_alg,
                signature: sig.to_vec(),
            },
            end,
        ))
    };
    let (sct, end) = parse().map_err(|e| format!("error parsing SCT: {e}"))?;
    if end < val.len() {
        return Err(format!(
            "extra data ({} bytes) after serialized SCT",
            val.len() - end
        ));
    }
    Ok(sct)
}

// ---------------------------------------------------------------------------------------
// The precertificate, its signed input, and the signature.

fn int_der(v: &[u8]) -> Vec<u8> {
    crate::der::tlv(0x02, v)
}

fn oid_der(arcs: &[i64]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut push = |mut v: i64| {
        let mut chunk = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            chunk.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        chunk.reverse();
        body.extend(chunk);
    };
    match arcs {
        [a, b, rest @ ..] => {
            push(a * 40 + b);
            for &x in rest {
                push(x);
            }
        }
        [a] => push(a * 40),
        [] => {}
    }
    crate::der::tlv(0x06, &body)
}

fn ai_der(ai: &(Vec<i64>, Option<&[u8]>)) -> Vec<u8> {
    let mut body = oid_der(&ai.0);
    if let Some(p) = ai.1 {
        body.extend_from_slice(p);
    }
    crate::der::tlv(0x30, &body)
}

/// encoding/asn1's Marshal of a time.Time field: UTCTime for 1950 to 2049, else
/// GeneralizedTime, in the time's zone; None where it cannot be represented.
fn time_der(t: &Time) -> Option<Vec<u8>> {
    let local = t.secs + i64::from(t.offset);
    let (y, m, d) = gotime::civil_from_days(local.div_euclid(86_400));
    let rem = local.rem_euclid(86_400);
    let (tag, mut s) = if (1950..2050).contains(&y) {
        (0x17, format!("{:02}", y.rem_euclid(100)))
    } else if (0..=9999).contains(&y) {
        (0x18, format!("{y:04}"))
    } else {
        return None;
    };
    s.push_str(&format!(
        "{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    ));
    let minutes = t.offset / 60;
    if minutes == 0 {
        s.push('Z');
    } else {
        s.push(if t.offset > 0 { '+' } else { '-' });
        let m = minutes.unsigned_abs();
        s.push_str(&format!("{:02}{:02}", m / 60, m % 60));
    }
    Some(crate::der::tlv(tag, s.as_bytes()))
}

fn bitstring_der(tag: u8, b: (&[u8], usize)) -> Vec<u8> {
    let mut body = vec![((8 - b.1 % 8) % 8) as u8];
    body.extend_from_slice(b.0);
    crate::der::tlv(tag, &body)
}

/// RemoveSCTList: the TBSCertificate read strictly, its one SCT list extension removed,
/// marshalled again as encoding/asn1 marshals the struct.
fn remove_sct_list(raw_tbs: &[u8]) -> Option<Vec<u8>> {
    let (v, rest) = unmarshal(&TBS_TY, raw_tbs, false).ok()?;
    if !rest.is_empty() {
        return None;
    }
    let tbs = tbs_of(&v)?;
    let exts = tbs.extensions.as_ref()?;
    let at: Vec<usize> = exts
        .iter()
        .enumerate()
        .filter(|(_, e)| arcs(&e.0, &OID_CT_SCT))
        .map(|(i, _)| i)
        .collect();
    let [at] = at.as_slice() else {
        return None;
    };
    let mut body = Vec::new();
    if tbs.version != 0 {
        let v = tbs.version.to_be_bytes();
        let i = (0..7)
            .find(|&i| {
                let (a, b) = (v.get(i).copied().unwrap_or(0), v.get(i + 1).copied().unwrap_or(0));
                !((a == 0 && b & 0x80 == 0) || (a == 0xff && b & 0x80 != 0))
            })
            .unwrap_or(7);
        body.extend(crate::der::tlv(0xa0, &int_der(v.get(i..).unwrap_or_default())));
    }
    body.extend(int_der(tbs.serial));
    body.extend(ai_der(&tbs.sig_alg));
    body.extend_from_slice(tbs.issuer);
    let validity = [time_der(&tbs.not_before)?, time_der(&tbs.not_after)?].concat();
    body.extend(crate::der::tlv(0x30, &validity));
    body.extend_from_slice(tbs.subject);
    body.extend_from_slice(tbs.spki);
    if let Some(u) = tbs.unique_id {
        body.extend(bitstring_der(0x81, u));
    }
    if let Some(u) = tbs.subject_unique_id {
        body.extend(bitstring_der(0x82, u));
    }
    let mut list = Vec::new();
    for (i, (id, critical, value)) in exts.iter().enumerate() {
        if i == *at {
            continue;
        }
        let mut e = oid_der(id);
        if *critical {
            e.extend_from_slice(&[0x01, 0x01, 0xff]);
        }
        e.extend(crate::der::tlv(0x04, value));
        list.extend(crate::der::tlv(0x30, &e));
    }
    body.extend(crate::der::tlv(0xa3, &crate::der::tlv(0x30, &list)));
    Some(crate::der::tlv(0x30, &body))
}

/// SerializeSCTSignatureInput for an embedded SCT (a precertificate entry).
fn signature_input(sct: &Sct, issuer_key_hash: &[u8], tbs: &[u8]) -> Option<Vec<u8>> {
    if sct.version != 0 || tbs.is_empty() || tbs.len() > 0xff_ffff {
        return None;
    }
    let mut out = vec![0u8, 0u8];
    out.extend_from_slice(&sct.timestamp.to_be_bytes());
    out.extend_from_slice(&[0, 1]);
    out.extend_from_slice(issuer_key_hash);
    let len = tbs.len() as u32;
    out.extend_from_slice(len.to_be_bytes().get(1..).unwrap_or_default());
    out.extend_from_slice(tbs);
    out.extend_from_slice(&(sct.extensions.len() as u16).to_be_bytes());
    out.extend_from_slice(&sct.extensions);
    Some(out)
}

/// The digest generateHash makes (MD5 refused, D105).
fn digest(alg: u64, data: &[u8]) -> Option<(crate::x509::Hash, Vec<u8>)> {
    use crate::x509::Hash;
    let h = match alg {
        2 => Hash::Sha1,
        3 => Hash::Sha224,
        4 => Hash::Sha256,
        5 => Hash::Sha384,
        6 => Hash::Sha512,
        _ => return None,
    };
    Some((h, h.of(data)))
}

/// ct.NewSignatureVerifier's acceptance of a log's key, then tls.VerifySignature.
fn verify_signature(key: &PublicKey, data: &[u8], sct: &Sct) -> bool {
    let rsa_ok = |n: &[u8]| num_bigint::BigUint::from_bytes_be(n).bits() >= 2048;
    match key {
        PublicKey::Rsa { n, .. } if rsa_ok(n) => {}
        PublicKey::Ecdsa {
            curve: Curve::P256, ..
        } => {}
        _ => return false,
    }
    let Some((hash, hashed)) = digest(sct.hash, data) else {
        return false;
    };
    match (sct.signature_alg, key) {
        (1, PublicKey::Rsa { n, e }) => hash
            .gitsign()
            .is_some_and(|g| shards_gitsign::arith::rsa_pkcs1_verify(n, e, g, &hashed, &sct.signature)),
        (3, PublicKey::Ecdsa { curve, point }) => {
            let Some((r, s)) = dsa_sig(&sct.signature) else {
                return false;
            };
            crate::x509::ecdsa_verify(*curve, point, &hashed, &r, &s)
        }
        _ => false,
    }
}

const DSA_SIG: [Field; 2] = [f("R", Ty::Big, NONE), f("S", Ty::Big, NONE)];

/// asn1.Unmarshal of a dsaSig{R, S *big.Int} (trailing data allowed), both positive:
/// their magnitudes.
fn dsa_sig(sig: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (v, _) = unmarshal(&Ty::Struct(&DSA_SIG, "dsaSig"), sig, false).ok()?;
    let V::Struct { fields, .. } = v else {
        return None;
    };
    let positive = |v: Option<&Option<V<'_>>>| match v {
        Some(Some(V::Big(b))) if b.first().is_some_and(|x| x & 0x80 == 0) && b.iter().any(|x| *x != 0) => {
            let i = b.iter().position(|x| *x != 0).unwrap_or(0);
            Some(b.get(i..).unwrap_or_default().to_vec())
        }
        _ => None,
    };
    Some((positive(fields.first())?, positive(fields.get(1))?))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// TimestampToTime.
fn timestamp_time(ts: u64) -> Time {
    let secs = i64::try_from(ts / 1000).unwrap_or(i64::MAX);
    let ms = u32::try_from(ts % 1000).unwrap_or(0);
    Time::utc(secs, ms * 1_000_000)
}

/// VerifySignedCertificateTimestamp: the leaf of the first chain's SCTs, each log's
/// counted once where one of the chains' issuers verifies it, at least `threshold`.
pub fn verify_scts(
    chains: &[Vec<&Certificate>],
    threshold: usize,
    ctlogs: &BTreeMap<String, TransparencyLog>,
) -> Result<(), String> {
    let Some(leaf) = chains.first().and_then(|c| c.first()) else {
        return Err("no chains provided".into());
    };
    let parsed = ct_parse(&leaf.raw);
    if let Some(e) = parsed.error() {
        return Err(format!("failed to parse certificate: {e}"));
    }
    let mut scts = Vec::new();
    for (i, val) in parsed.sct_list.iter().enumerate() {
        scts.push(extract_sct(val).map_err(|e| format!("error extracting SCT number {i}: {e}"))?);
    }
    let mut verified: Vec<String> = Vec::new();
    for sct in &scts {
        let id = hex(&sct.log_id);
        if verified.contains(&id) {
            continue;
        }
        let Some(log) = ctlogs.get(&id) else {
            continue;
        };
        let when = timestamp_time(sct.timestamp);
        if log.start.is_some_and(|s| !s.is_zero() && when < s) {
            continue;
        }
        if log.end.is_some_and(|e| !e.is_zero() && when > e) {
            continue;
        }
        for chain in chains {
            let Some(issuer) = chain.get(1) else {
                continue;
            };
            if ct_parse(&issuer.raw).error().is_some() {
                continue;
            }
            let Some(tbs) = remove_sct_list(&leaf.raw_tbs) else {
                continue;
            };
            let key_hash = crate::x509::Hash::Sha256.of(&issuer.raw_spki);
            let Some(input) = signature_input(sct, &key_hash, &tbs) else {
                continue;
            };
            if verify_signature(&log.key, &input, sct) {
                verified.push(id.clone());
                break;
            }
        }
    }
    if verified.len() < threshold {
        return Err(format!(
            "only able to verify {} SCT entries; unable to meet threshold of {threshold}",
            verified.len()
        ));
    }
    Ok(())
}
