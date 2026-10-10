//! Rekor v2 entry bodies as sigstore-go reads them (unmarshalRekorV2Entry): protojson of
//! rekor-tiles' Entry, taken only where it is a hashedRekordV002 of apiVersion 0.0.2 (any
//! other body is read as Rekor v1, so protojson's verdict is what matters here, not its
//! words); its validation (validateHashedRekordV002Entry, typesverifier.Validate); and the
//! leaf hash rekor-tiles' hashedrekord.ToEntryHash makes of a reconstructed entry.

use super::gocodec::{RAW_STD_ENCODING, RAW_URL_ENCODING, STD_ENCODING, URL_ENCODING, std_encode};
use crate::keys::Details;

/// A JSON value as protojson's decoder reads it: numbers as written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PValue {
    Null,
    Bool,
    Number(String),
    Str(String),
    Array(Vec<PValue>),
    Object(Vec<(String, PValue)>),
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

/// The deepest an Entry's values go (`dsseV002.signatures[].verifier.publicKey.rawBytes`):
/// its schema has no field of any shape (no Struct, Value or Any), so a body nested deeper
/// is no Entry, which protojson finds at the first field its schema lacks. Refused at
/// this depth, a body is never read deeper, and the parse's recursion stays this shallow
/// on any thread.
const MAX_DEPTH: usize = 7;

fn not_delim(c: u8) -> bool {
    matches!(c, b'-' | b'+' | b'.' | b'_') || c.is_ascii_alphanumeric()
}

impl Parser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.at), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        let rest = self.b.get(self.at..)?;
        if !rest.starts_with(word) || rest.get(word.len()).is_some_and(|c| not_delim(*c)) {
            return None;
        }
        self.at += word.len();
        Some(())
    }

    /// parseNumber.
    fn number(&mut self) -> Option<String> {
        let input = self.b.get(self.at..)?;
        let mut n = 0;
        let mut s = input;
        if s.first() == Some(&b'-') {
            s = s.get(1..)?;
            n += 1;
            if s.is_empty() {
                return None;
            }
        }
        match s.first() {
            Some(b'0') => {
                s = s.get(1..)?;
                n += 1;
            }
            Some(b'1'..=b'9') => {
                s = s.get(1..)?;
                n += 1;
                while s.first().is_some_and(u8::is_ascii_digit) {
                    s = s.get(1..)?;
                    n += 1;
                }
            }
            _ => return None,
        }
        if s.len() >= 2 && s.first() == Some(&b'.') && s.get(1).is_some_and(u8::is_ascii_digit) {
            s = s.get(2..)?;
            n += 2;
            while s.first().is_some_and(u8::is_ascii_digit) {
                s = s.get(1..)?;
                n += 1;
            }
        }
        if s.len() >= 2 && matches!(s.first(), Some(b'e' | b'E')) {
            s = s.get(1..)?;
            n += 1;
            if matches!(s.first(), Some(b'+' | b'-')) {
                s = s.get(1..)?;
                n += 1;
                if s.is_empty() {
                    return None;
                }
            }
            while s.first().is_some_and(u8::is_ascii_digit) {
                s = s.get(1..)?;
                n += 1;
            }
        }
        if input.get(n).is_some_and(|c| not_delim(*c)) {
            return None;
        }
        self.at += n;
        Some(String::from_utf8_lossy(input.get(..n)?).into_owned())
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let h = self.b.get(at..at + 4)?;
        if !h.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
    }

    /// parseString: valid UTF-8, no control characters, escapes as JSON has them, a
    /// surrogate only in a valid pair.
    fn string(&mut self) -> Option<String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let rest = self.b.get(self.at..)?;
            let &c = rest.first()?;
            match c {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    let &e = rest.get(1)?;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(char::from(e)),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let r = self.hex4(self.at + 2)?;
                            self.at += 6;
                            if (0xd800..0xe000).contains(&r) {
                                if self.b.get(self.at) != Some(&b'\\')
                                    || self.b.get(self.at + 1) != Some(&b'u')
                                {
                                    return None;
                                }
                                let low = self.hex4(self.at + 2)?;
                                if !(0xd800..0xdc00).contains(&r) || !(0xdc00..0xe000).contains(&low) {
                                    return None;
                                }
                                self.at += 6;
                                out.push(char::from_u32(0x10000 + ((r - 0xd800) << 10) + (low - 0xdc00))?);
                            } else {
                                out.push(char::from_u32(r)?);
                            }
                            continue;
                        }
                        _ => return None,
                    }
                    self.at += 2;
                }
                c if c < 0x20 => return None,
                _ => {
                    let w =
                        (1..=4).find(|w| rest.get(..*w).is_some_and(|h| std::str::from_utf8(h).is_ok()))?;
                    out.push_str(std::str::from_utf8(rest.get(..w)?).ok()?);
                    self.at += w;
                }
            }
        }
    }

    fn value(&mut self, depth: usize) -> Option<PValue> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.ws();
        match self.b.get(self.at)? {
            b'n' => self.literal(b"null").map(|()| PValue::Null),
            b't' => self.literal(b"true").map(|()| PValue::Bool),
            b'f' => self.literal(b"false").map(|()| PValue::Bool),
            b'"' => self.string().map(PValue::Str),
            b'-' | b'0'..=b'9' => self.number().map(PValue::Number),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Some(PValue::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.at)? {
                        b',' => self.at += 1,
                        b']' => {
                            self.at += 1;
                            return Some(PValue::Array(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                self.ws();
                if self.b.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Some(PValue::Object(members));
                }
                loop {
                    self.ws();
                    if self.b.get(self.at) != Some(&b'"') {
                        return None;
                    }
                    let name = self.string()?;
                    self.ws();
                    if self.b.get(self.at) != Some(&b':') {
                        return None;
                    }
                    self.at += 1;
                    let v = self.value(depth + 1)?;
                    members.push((name, v));
                    self.ws();
                    match self.b.get(self.at)? {
                        b',' => self.at += 1,
                        b'}' => {
                            self.at += 1;
                            return Some(PValue::Object(members));
                        }
                        _ => return None,
                    }
                }
            }
            _ => None,
        }
    }
}

fn parse(b: &[u8]) -> Option<PValue> {
    let mut p = Parser { b, at: 0 };
    let v = p.value(0)?;
    p.ws();
    (p.at == b.len()).then_some(v)
}

/// The names HashAlgorithm's values have, by number.
pub const HASH_ALGORITHM: [(&str, i32); 6] = [
    ("HASH_ALGORITHM_UNSPECIFIED", 0),
    ("SHA2_256", 1),
    ("SHA2_384", 2),
    ("SHA2_512", 3),
    ("SHA3_256", 4),
    ("SHA3_384", 5),
];

/// The names PublicKeyDetails' values have, by number.
pub const PUBLIC_KEY_DETAILS: [(&str, i32); 24] = [
    ("PUBLIC_KEY_DETAILS_UNSPECIFIED", 0),
    ("PKCS1_RSA_PKCS1V5", 1),
    ("PKCS1_RSA_PSS", 2),
    ("PKIX_RSA_PKCS1V5", 3),
    ("PKIX_RSA_PSS", 4),
    ("PKIX_RSA_PKCS1V15_2048_SHA256", 9),
    ("PKIX_RSA_PKCS1V15_3072_SHA256", 10),
    ("PKIX_RSA_PKCS1V15_4096_SHA256", 11),
    ("PKIX_RSA_PSS_2048_SHA256", 16),
    ("PKIX_RSA_PSS_3072_SHA256", 17),
    ("PKIX_RSA_PSS_4096_SHA256", 18),
    ("PKIX_ECDSA_P256_HMAC_SHA_256", 6),
    ("PKIX_ECDSA_P256_SHA_256", 5),
    ("PKIX_ECDSA_P384_SHA_384", 12),
    ("PKIX_ECDSA_P521_SHA_512", 13),
    ("PKIX_ED25519", 7),
    ("PKIX_ED25519_PH", 8),
    ("PKIX_ECDSA_P384_SHA_256", 19),
    ("PKIX_ECDSA_P521_SHA_256", 20),
    ("LMS_SHA256", 14),
    ("LMOTS_SHA256", 15),
    ("ML_DSA_44", 23),
    ("ML_DSA_65", 21),
    ("ML_DSA_87", 22),
];

/// An enum value's String: its name, or its number where it has none.
pub fn enum_string(names: &[(&str, i32)], v: i32) -> String {
    names
        .iter()
        .find(|(_, n)| *n == v)
        .map_or_else(|| v.to_string(), |(name, _)| (*name).to_string())
}

/// A field of a message: its JSON and proto names, its number, its kind.
#[derive(Clone, Copy)]
enum FieldKind {
    Str,
    Bytes,
    Enum(&'static [(&'static str, i32)]),
    Msg(&'static [Field]),
}

#[derive(Clone, Copy)]
struct Field {
    json: &'static str,
    proto: &'static str,
    number: u32,
    kind: FieldKind,
    repeated: bool,
    /// The oneof the field is in, if any.
    oneof: Option<u8>,
}

const fn f(json: &'static str, proto: &'static str, number: u32, kind: FieldKind) -> Field {
    Field {
        json,
        proto,
        number,
        kind,
        repeated: false,
        oneof: None,
    }
}

const fn one(mut x: Field, o: u8) -> Field {
    x.oneof = Some(o);
    x
}

const fn rep(mut x: Field) -> Field {
    x.repeated = true;
    x
}

const RAW_BYTES: [Field; 1] = [f("rawBytes", "raw_bytes", 1, FieldKind::Bytes)];
const VERIFIER: [Field; 3] = [
    one(f("publicKey", "public_key", 1, FieldKind::Msg(&RAW_BYTES)), 0),
    one(
        f(
            "x509Certificate",
            "x509_certificate",
            2,
            FieldKind::Msg(&RAW_BYTES),
        ),
        0,
    ),
    f(
        "keyDetails",
        "key_details",
        3,
        FieldKind::Enum(&PUBLIC_KEY_DETAILS),
    ),
];
const SIGNATURE: [Field; 2] = [
    f("content", "content", 1, FieldKind::Bytes),
    f("verifier", "verifier", 2, FieldKind::Msg(&VERIFIER)),
];
const HASH_OUTPUT: [Field; 2] = [
    f("algorithm", "algorithm", 1, FieldKind::Enum(&HASH_ALGORITHM)),
    f("digest", "digest", 2, FieldKind::Bytes),
];
const HASHED_REKORD: [Field; 2] = [
    f("data", "data", 1, FieldKind::Msg(&HASH_OUTPUT)),
    f("signature", "signature", 2, FieldKind::Msg(&SIGNATURE)),
];
const DSSE: [Field; 2] = [
    f("payloadHash", "payloadHash", 1, FieldKind::Msg(&HASH_OUTPUT)),
    rep(f("signatures", "signatures", 2, FieldKind::Msg(&SIGNATURE))),
];
const SPEC: [Field; 2] = [
    one(
        f(
            "hashedRekordV002",
            "hashed_rekord_v002",
            1,
            FieldKind::Msg(&HASHED_REKORD),
        ),
        0,
    ),
    one(f("dsseV002", "dsse_v002", 2, FieldKind::Msg(&DSSE)), 0),
];
const ENTRY: [Field; 3] = [
    f("kind", "kind", 1, FieldKind::Str),
    f("apiVersion", "api_version", 2, FieldKind::Str),
    f("spec", "spec", 3, FieldKind::Msg(&SPEC)),
];

/// A decoded message: each field set, by number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Msg {
    fields: Vec<(u32, Val)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    Str(String),
    Bytes(Vec<u8>),
    Enum(i32),
    Msg(Msg),
    List,
}

impl Msg {
    fn get(&self, n: u32) -> Option<&Val> {
        self.fields.iter().find(|(k, _)| *k == n).map(|(_, v)| v)
    }

    fn msg(&self, n: u32) -> Option<&Msg> {
        match self.get(n)? {
            Val::Msg(m) => Some(m),
            _ => None,
        }
    }

    fn bytes(&self, n: u32) -> Vec<u8> {
        match self.get(n) {
            Some(Val::Bytes(b)) => b.clone(),
            _ => Vec::new(),
        }
    }

    fn string(&self, n: u32) -> String {
        match self.get(n) {
            Some(Val::Str(s)) => s.clone(),
            _ => String::new(),
        }
    }

    fn enumeration(&self, n: u32) -> i32 {
        match self.get(n) {
            Some(Val::Enum(e)) => *e,
            _ => 0,
        }
    }
}

/// unmarshalBytes.
fn proto_bytes(s: &str) -> Option<Vec<u8>> {
    let url = s.contains(['-', '_']);
    let enc = match (url, !s.len().is_multiple_of(4)) {
        (false, false) => STD_ENCODING,
        (true, false) => URL_ENCODING,
        (false, true) => RAW_STD_ENCODING,
        (true, true) => RAW_URL_ENCODING,
    };
    enc.decode(s.as_bytes()).ok()
}

/// A JSON number token as an int32 (Token.Int(32)).
fn int32(raw: &str) -> Option<i32> {
    let b = raw.as_bytes();
    let mut s = b;
    let neg = s.first() == Some(&b'-');
    if neg {
        s = s.get(1..)?;
    }
    let mut intp: &[u8] = &[];
    match s.first()? {
        b'0' => s = s.get(1..)?,
        b'1'..=b'9' => {
            let n = s.iter().take_while(|c| c.is_ascii_digit()).count();
            intp = s.get(..n)?;
            s = s.get(n..)?;
        }
        _ => return None,
    }
    let mut frac: &[u8] = &[];
    if s.len() >= 2 && s.first() == Some(&b'.') && s.get(1).is_some_and(u8::is_ascii_digit) {
        let n = s.get(1..)?.iter().take_while(|c| c.is_ascii_digit()).count();
        frac = s.get(1..1 + n)?;
        s = s.get(1 + n..)?;
    }
    while let Some((&b'0', head)) = frac.split_last() {
        frac = head;
    }
    let mut exp: &[u8] = &[];
    if s.len() >= 2 && matches!(s.first(), Some(b'e' | b'E')) {
        let t = s.get(1..)?;
        let mut n = 0;
        if matches!(t.first(), Some(b'+' | b'-')) {
            n += 1;
        }
        n += t.get(n..)?.iter().take_while(|c| c.is_ascii_digit()).count();
        exp = t.get(..n)?;
    }
    if intp.is_empty() && frac.is_empty() {
        return Some(0);
    }
    let e: i64 = if exp.is_empty() {
        0
    } else {
        let t = std::str::from_utf8(exp).ok()?;
        let v: i64 = t.parse().ok()?;
        i32::try_from(v).ok()?;
        v
    };
    let mut num: Vec<u8>;
    if e >= 0 {
        let e = usize::try_from(e).ok()?;
        if frac.len() > e || intp.len() + e > 20 {
            return None;
        }
        num = intp.to_vec();
        num.extend_from_slice(frac);
        num.extend(std::iter::repeat_n(b'0', e - frac.len()));
    } else {
        if !frac.is_empty() {
            return None;
        }
        let index = i64::try_from(intp.len()).ok()? + e;
        let index = usize::try_from(index).ok()?;
        if intp.get(index..)?.iter().any(|c| *c != b'0') {
            return None;
        }
        num = intp.get(..index)?.to_vec();
    }
    let text = String::from_utf8(num).ok()?;
    let v: i64 = if text.is_empty() {
        // strconv.ParseInt("") fails.
        return None;
    } else {
        text.parse().ok()?
    };
    i32::try_from(if neg { -v } else { v }).ok()
}

/// unmarshalMessage against `schema`: the message, or None where protojson refuses it.
fn message(v: &PValue, schema: &[Field]) -> Option<Msg> {
    let PValue::Object(members) = v else {
        return None;
    };
    let mut out = Msg::default();
    let mut seen: Vec<u32> = Vec::new();
    let mut oneofs: Vec<u8> = Vec::new();
    for (name, val) in members {
        let fd = schema
            .iter()
            .find(|fd| fd.json == name)
            .or_else(|| schema.iter().find(|fd| fd.proto == name))?;
        if seen.contains(&fd.number) {
            return None;
        }
        seen.push(fd.number);
        if *val == PValue::Null {
            continue;
        }
        if fd.repeated {
            let PValue::Array(items) = val else {
                return None;
            };
            for item in items {
                if let FieldKind::Msg(s) = fd.kind {
                    message(item, s)?;
                }
            }
            out.fields.push((fd.number, Val::List));
            continue;
        }
        if let Some(o) = fd.oneof {
            if oneofs.contains(&o) {
                return None;
            }
            oneofs.push(o);
        }
        let got = match (fd.kind, val) {
            (FieldKind::Str, PValue::Str(s)) => Val::Str(s.clone()),
            (FieldKind::Bytes, PValue::Str(s)) => Val::Bytes(proto_bytes(s)?),
            (FieldKind::Enum(names), PValue::Str(s)) => Val::Enum(names.iter().find(|(n, _)| n == s)?.1),
            (FieldKind::Enum(_), PValue::Number(n)) => Val::Enum(int32(n)?),
            (FieldKind::Msg(s), m) => Val::Msg(message(m, s)?),
            _ => return None,
        };
        out.fields.push((fd.number, got));
    }
    Some(out)
}

/// A Rekor v2 verifier as the entry gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V2Verifier {
    /// publicKey's rawBytes (a PKIX key).
    PublicKey(Vec<u8>),
    /// x509Certificate's rawBytes.
    Certificate(Vec<u8>),
}

/// A signature's verifier message: the key or certificate it holds (None where it
/// holds neither) and its key details.
pub type V2VerifierMsg = (Option<V2Verifier>, i32);

/// HashedRekordLogEntryV002 as the entry gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2 {
    /// data: its algorithm and digest, where present.
    pub data: Option<(i32, Vec<u8>)>,
    /// signature: its content and verifier, where present.
    pub signature: Option<(Vec<u8>, Option<V2VerifierMsg>)>,
}

/// unmarshalRekorV2Entry: the entry, where the body is a hashedRekordV002 of
/// apiVersion 0.0.2.
pub fn unmarshal(body: &[u8]) -> Option<V2> {
    let v = parse(body)?;
    let entry = message(&v, &ENTRY)?;
    let spec = entry.msg(3)?;
    let hr = spec.msg(1)?;
    if entry.string(2) != "0.0.2" {
        return None;
    }
    let data = hr.msg(1).map(|d| (d.enumeration(1), d.bytes(2)));
    let signature = hr.msg(2).map(|s| {
        let verifier = s.msg(2).map(|v| {
            let which = if let Some(pk) = v.msg(1) {
                Some(V2Verifier::PublicKey(pk.bytes(1)))
            } else {
                v.msg(2).map(|c| V2Verifier::Certificate(c.bytes(1)))
            };
            (which, v.enumeration(3))
        });
        (s.bytes(1), verifier)
    });
    Some(V2 { data, signature })
}

/// validateHashedRekordV002Entry.
pub fn validate(e: &V2) -> Result<(), String> {
    let Some((content, verifier)) = &e.signature else {
        return Err("missing signature".into());
    };
    if content.is_empty() {
        return Err("missing signature".into());
    }
    let Some((which, _)) = verifier else {
        return Err("missing verifier".into());
    };
    if e.data.is_none() {
        return Err("missing digest".into());
    }
    match which {
        None => Err("missing signature public key or X.509 certificate".into()),
        Some(V2Verifier::PublicKey(raw)) if raw.is_empty() => Err("missing public key raw bytes".into()),
        Some(V2Verifier::Certificate(raw)) if raw.is_empty() => {
            Err("missing X.509 certificate raw bytes".into())
        }
        _ => Ok(()),
    }
}

/// The proto hash a key's details hash with (GetProtoHashType).
fn proto_hash(d: Details) -> i32 {
    match d {
        Details::EcdsaP384Sha384 => 2,
        Details::EcdsaP521Sha512 | Details::Ed25519Ph => 3,
        Details::Ed25519 => 0,
        _ => 1,
    }
}

/// hashedrekord.ToEntryHash: the RFC 6962 leaf hash of the canonical JSON of the entry
/// rebuilt from the digest, signature, verifier and its key details.
pub fn to_entry_hash(
    digest: &[u8],
    signature: &[u8],
    verifier: &V2Verifier,
    details: Details,
) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let alg = proto_hash(details);
    if alg != 0 {
        data.push(format!("\"algorithm\":\"{}\"", enum_string(&HASH_ALGORITHM, alg)));
    }
    if !digest.is_empty() {
        data.push(format!("\"digest\":\"{}\"", std_encode(digest)));
    }
    let (which, raw) = match verifier {
        V2Verifier::PublicKey(r) => ("publicKey", r),
        V2Verifier::Certificate(r) => ("x509Certificate", r),
    };
    let inner = if raw.is_empty() {
        String::new()
    } else {
        format!("\"rawBytes\":\"{}\"", std_encode(raw))
    };
    let mut ver = vec![format!("\"{which}\":{{{inner}}}")];
    let (name, _) = details.proto();
    ver.push(format!("\"keyDetails\":\"{name}\""));
    let mut sig = Vec::new();
    if !signature.is_empty() {
        sig.push(format!("\"content\":\"{}\"", std_encode(signature)));
    }
    sig.push(format!("\"verifier\":{{{}}}", ver.join(",")));
    let json = format!(
        "{{\"kind\":\"hashedrekord\",\"apiVersion\":\"0.0.2\",\"spec\":{{\"hashedRekordV002\":{{\"data\":{{{}}},\"signature\":{{{}}}}}}}}}",
        data.join(","),
        sig.join(",")
    );
    let canonical = super::jcs::transform(json.as_bytes())
        .map_err(|e| format!("canonicalizing reconstructed entry: {e}"))?;
    Ok(super::note::hash_leaf(&canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protojson_s_verdicts() {
        let ok = br#"{"kind":"hashedrekord","apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{"algorithm":"SHA2_256","digest":"AAEC"},"signature":{"content":"AQ","verifier":{"publicKey":{"rawBytes":"AQ=="},"keyDetails":5}}}}}"#;
        let e = unmarshal(ok).unwrap();
        assert_eq!(e.data, Some((1, vec![0, 1, 2])));
        assert!(unmarshal(br#"{"kind":"x","apiVersion":"0.0.1","spec":{"hashedRekordV002":{}}}"#).is_none());
        assert!(unmarshal(br#"{"apiVersion":"0.0.2","spec":{"dsseV002":{}}}"#).is_none());
        assert!(
            unmarshal(br#"{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{},"dsseV002":{}}}"#).is_none()
        );
        assert!(
            unmarshal(br#"{"apiVersion":"0.0.2","api_version":"0.0.2","spec":{"hashedRekordV002":{}}}"#)
                .is_none()
        );
        assert!(unmarshal(br#"{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{}},"x":1}"#).is_none());
        assert!(
            unmarshal(br#"{"apiVersion":"0.0.2","spec":{"hashed_rekord_v002":{"data":null}}}"#).is_some()
        );
        assert_eq!(int32("1e1"), Some(10));
        assert_eq!(int32("1.5"), None);
        assert_eq!(int32("1e"), Some(1));
        assert_eq!(int32("10e-1"), Some(1));
    }

    /// How deep the values of a message of `schema` at `depth` go.
    fn deepest(schema: &[Field], depth: usize) -> usize {
        let mut max = depth;
        for f in schema {
            let at = depth + 1 + usize::from(f.repeated);
            max = max.max(match f.kind {
                FieldKind::Msg(s) => deepest(s, at),
                _ => at,
            });
        }
        max
    }

    /// An Entry nests no deeper than its schema, which `MAX_DEPTH` is; a body nested
    /// deeper (any mirror's, before anything is verified) is refused before its depth is
    /// read, on a stack too small for recursion a level at a time.
    #[test]
    fn bodies_nest_no_deeper_than_an_entry() {
        assert_eq!(deepest(&ENTRY, 0), MAX_DEPTH);
        let deepest = br#"{"apiVersion":"0.0.2","spec":{"dsseV002":{"signatures":[{"verifier":{"publicKey":{"rawBytes":"AQ=="}}}]}}}"#;
        assert!(parse(deepest).and_then(|v| message(&v, &ENTRY)).is_some());
        let verdicts = std::thread::Builder::new()
            .stack_size(64 << 10)
            .spawn(|| {
                let deep = [b"[".repeat(10_000), b"]".repeat(10_000)].concat();
                let deeper = br#"{"apiVersion":"0.0.2","spec":{"dsseV002":{"signatures":[{"verifier":{"publicKey":{"rawBytes":["AQ=="]}}}]}}}"#;
                (unmarshal(&deep), parse(deeper))
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(verdicts, (None, None));
    }
}
