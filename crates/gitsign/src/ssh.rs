//! SSH public keys as golang.org/x/crypto v0.55.0's `ssh.ParsePublicKey` and
//! `ssh.ParseAuthorizedKey` read them, and SSH signatures (PROTOCOL.sshsig) as
//! hiddeco/sshsig's `ParseSignature` reads them and its `Verify` verifies them: each key
//! type's checks, its fingerprint (`ssh.FingerprintSHA256`) of the key as it marshals it
//! again, and each key type's `Verify`.

use base64::Engine as _;
use sha2::Digest as _;

/// An SSH wire reader: `uint32` and `string` (RFC 4251 §5).
struct Wire<'a>(&'a [u8]);

impl<'a> Wire<'a> {
    fn u32(&mut self) -> Option<u32> {
        let (n, rest) = self.0.split_first_chunk::<4>()?;
        self.0 = rest;
        Some(u32::from_be_bytes(*n))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u32()?).ok()?;
        let (s, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(s)
    }

    /// parseInt: an mpint, two's complement, as a sign and a magnitude.
    fn int(&mut self) -> Option<Int> {
        let b = self.string()?;
        Some(Int::of(b))
    }
}

/// x/crypto's errors for a short message and a malformed one.
const SHORT: &str = "ssh: short read";

/// A big integer as Go's big.Int holds one parsed from an mpint: negative or not, and its
/// magnitude without leading zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Int {
    pub negative: bool,
    pub magnitude: Vec<u8>,
}

impl Int {
    fn of(b: &[u8]) -> Int {
        if b.first().is_some_and(|x| x & 0x80 != 0) {
            // -(^b + 1)
            let mut m: Vec<u8> = b.iter().map(|x| !x).collect();
            for x in m.iter_mut().rev() {
                let (v, carry) = x.overflowing_add(1);
                *x = v;
                if !carry {
                    break;
                }
            }
            if m.iter().all(|x| *x == 0) {
                m.insert(0, 1);
            }
            return Int {
                negative: true,
                magnitude: strip(&m).to_vec(),
            };
        }
        Int {
            negative: false,
            magnitude: strip(b).to_vec(),
        }
    }

    fn bit_len(&self) -> usize {
        match self.magnitude.first() {
            None => 0,
            Some(&top) => (self.magnitude.len() - 1) * 8 + (8 - top.leading_zeros() as usize),
        }
    }

    fn is_zero(&self) -> bool {
        self.magnitude.is_empty()
    }

    /// `x.Int64()` of an integer of at most 24 bits.
    fn small(&self) -> i64 {
        let v = self.magnitude.iter().fold(0i64, |a, &b| (a << 8) | i64::from(b));
        if self.negative { -v } else { v }
    }

    /// Marshal's mpint (RFC 4251 §5): two's complement, minimal.
    fn mpint(&self) -> Vec<u8> {
        if self.is_zero() {
            return Vec::new();
        }
        if !self.negative {
            let mut out = Vec::with_capacity(self.magnitude.len() + 1);
            if self.magnitude.first().is_some_and(|x| x & 0x80 != 0) {
                out.push(0);
            }
            out.extend_from_slice(&self.magnitude);
            return out;
        }
        // Two's complement of the magnitude, minus one then inverted.
        let mut m = self.magnitude.clone();
        for x in m.iter_mut().rev() {
            let (v, borrow) = x.overflowing_sub(1);
            *x = v;
            if !borrow {
                break;
            }
        }
        let mut out: Vec<u8> = m.iter().map(|x| !x).collect();
        while out.len() > 1 && out.first() == Some(&0xff) && out.get(1).is_some_and(|x| x & 0x80 != 0) {
            out.remove(0);
        }
        if out.first().is_none_or(|x| x & 0x80 == 0) {
            out.insert(0, 0xff);
        }
        out
    }

    /// `x.Cmp(y)` of two non-negative integers.
    fn lt(&self, other: &Int) -> bool {
        (self.magnitude.len(), &self.magnitude) < (other.magnitude.len(), &other.magnitude)
    }
}

fn strip(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    b.get(i..).unwrap_or_default()
}

/// A public key of a type x/crypto reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    Rsa { e: Int, n: Int },
    Dsa { p: Int, q: Int, g: Int, y: Int },
    Ecdsa { curve: &'static str, point: Vec<u8> },
    SkEcdsa { point: Vec<u8>, application: Vec<u8> },
    Ed25519(Vec<u8>),
    SkEd25519 { key: Vec<u8>, application: Vec<u8> },
}

fn put(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

impl PublicKey {
    /// Its type, as `Type()` names it.
    pub fn kind(&self) -> &'static str {
        match self {
            PublicKey::Rsa { .. } => "ssh-rsa",
            PublicKey::Dsa { .. } => "ssh-dss",
            PublicKey::Ecdsa {
                curve: "nistp256", ..
            } => "ecdsa-sha2-nistp256",
            PublicKey::Ecdsa {
                curve: "nistp384", ..
            } => "ecdsa-sha2-nistp384",
            PublicKey::Ecdsa { .. } => "ecdsa-sha2-nistp521",
            PublicKey::SkEcdsa { .. } => "sk-ecdsa-sha2-nistp256@openssh.com",
            PublicKey::Ed25519(_) => "ssh-ed25519",
            PublicKey::SkEd25519 { .. } => "sk-ssh-ed25519@openssh.com",
        }
    }

    /// `Marshal()`: the key in wire format, as each type writes itself.
    pub fn marshal(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, self.kind().as_bytes());
        match self {
            PublicKey::Rsa { e, n } => {
                put(&mut out, &e.mpint());
                put(&mut out, &n.mpint());
            }
            PublicKey::Dsa { p, q, g, y } => {
                for i in [p, q, g, y] {
                    put(&mut out, &i.mpint());
                }
            }
            PublicKey::Ecdsa { curve, point } => {
                put(&mut out, curve.as_bytes());
                put(&mut out, point);
            }
            PublicKey::SkEcdsa { point, application } => {
                put(&mut out, b"nistp256");
                put(&mut out, point);
                put(&mut out, application);
            }
            PublicKey::Ed25519(k) => put(&mut out, k),
            PublicKey::SkEd25519 { key, application } => {
                put(&mut out, key);
                put(&mut out, application);
            }
        }
        out
    }

    /// `ssh.FingerprintSHA256`.
    pub fn fingerprint(&self) -> String {
        let sum = sha2::Sha256::digest(self.marshal());
        format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(sum)
        )
    }
}

/// elliptic.Unmarshal's check: an uncompressed point of the curve, on the curve.
fn curve_point(curve: &str, b: &[u8]) -> bool {
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_FIXED, ECDSA_P384_SHA384_FIXED, ECDSA_P521_SHA512_FIXED, ParsedPublicKey,
    };
    let (size, alg) = match curve {
        "nistp256" => (32, &ECDSA_P256_SHA256_FIXED),
        "nistp384" => (48, &ECDSA_P384_SHA384_FIXED),
        _ => (66, &ECDSA_P521_SHA512_FIXED),
    };
    b.len() == 1 + 2 * size && b.first() == Some(&4) && ParsedPublicKey::new(alg, b).is_ok()
}

/// ssh.ParsePublicKey.
pub fn parse_public_key(input: &[u8]) -> Result<PublicKey, String> {
    let mut w = Wire(input);
    let algo = w.string().ok_or(SHORT)?;
    let algo = String::from_utf8_lossy(algo).into_owned();
    let mut fields = Wire(w.0);
    let key = match algo.as_str() {
        "ssh-rsa" => {
            let e = fields.int().ok_or(SHORT)?;
            let n = fields.int().ok_or(SHORT)?;
            if n.bit_len() > 16384 {
                return Err("ssh: rsa modulus too large".into());
            }
            if e.bit_len() > 24 {
                return Err("ssh: exponent too large".into());
            }
            let v = e.small();
            if v < 3 || v & 1 == 0 {
                return Err("ssh: incorrect exponent".into());
            }
            PublicKey::Rsa { e, n }
        }
        "ssh-dss" => {
            let p = fields.int().ok_or(SHORT)?;
            let q = fields.int().ok_or(SHORT)?;
            let g = fields.int().ok_or(SHORT)?;
            let y = fields.int().ok_or(SHORT)?;
            if p.bit_len() != 1024 {
                return Err(format!("ssh: unsupported DSA key size {}", p.bit_len()));
            }
            if y.negative || y.is_zero() || !y.lt(&p) {
                return Err("ssh: DSA public value Y out of range".into());
            }
            PublicKey::Dsa { p, q, g, y }
        }
        "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521" => {
            let curve = fields.string().ok_or(SHORT)?;
            let point = fields.string().ok_or(SHORT)?;
            let curve = String::from_utf8_lossy(curve).into_owned();
            let actual = format!("ecdsa-sha2-{curve}");
            if actual != algo {
                return Err(format!(
                    "ssh: algorithm type mismatch: expected {algo:?}, found curve {curve:?} (type {actual:?})"
                ));
            }
            let curve: &'static str = match curve.as_str() {
                "nistp256" => "nistp256",
                "nistp384" => "nistp384",
                "nistp521" => "nistp521",
                _ => return Err("ssh: unsupported curve".into()),
            };
            if !curve_point(curve, point) {
                return Err("ssh: invalid curve point".into());
            }
            PublicKey::Ecdsa {
                curve,
                point: point.to_vec(),
            }
        }
        "sk-ecdsa-sha2-nistp256@openssh.com" => {
            let curve = fields.string().ok_or(SHORT)?;
            let point = fields.string().ok_or(SHORT)?;
            let application = fields.string().ok_or(SHORT)?;
            if curve != b"nistp256" {
                return Err("ssh: unsupported curve".into());
            }
            if !curve_point("nistp256", point) {
                return Err("ssh: invalid curve point".into());
            }
            PublicKey::SkEcdsa {
                point: point.to_vec(),
                application: application.to_vec(),
            }
        }
        "ssh-ed25519" => {
            let key = fields.string().ok_or(SHORT)?;
            if key.len() != 32 {
                return Err(format!("invalid size {} for Ed25519 public key", key.len()));
            }
            PublicKey::Ed25519(key.to_vec())
        }
        "sk-ssh-ed25519@openssh.com" => {
            let key = fields.string().ok_or(SHORT)?;
            let application = fields.string().ok_or(SHORT)?;
            if key.len() != 32 {
                return Err(format!("invalid size {} for Ed25519 public key", key.len()));
            }
            PublicKey::SkEd25519 {
                key: key.to_vec(),
                application: application.to_vec(),
            }
        }
        a if a.ends_with("-cert-v01@openssh.com") => {
            return Err(format!("ssh: {a} certificates are not read by shards yet"));
        }
        a => {
            let format = match a {
                "rsa-sha2-256" | "rsa-sha2-512" => Some("ssh-rsa"),
                "rsa-sha2-256-cert-v01@openssh.com" | "rsa-sha2-512-cert-v01@openssh.com" => {
                    Some("ssh-rsa-cert-v01@openssh.com")
                }
                _ => None,
            };
            return Err(match format {
                Some(f) => format!(
                    "ssh: signature algorithm {a:?} isn't a key format; key is malformed and should be re-encoded with type {f:?}"
                ),
                None => format!("ssh: unknown key algorithm: {a}"),
            });
        }
    };
    if !fields.0.is_empty() {
        return Err("ssh: trailing junk in public key".into());
    }
    Ok(key)
}

/// An SSH signature (PROTOCOL.sshsig): its version, its signer's key, namespace and hash,
/// and the signature's format and blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub version: u32,
    pub public_key: PublicKey,
    pub namespace: Vec<u8>,
    pub hash_algorithm: Vec<u8>,
    pub format: Vec<u8>,
    pub blob: Vec<u8>,
    /// What follows the blob: a security key's flags and counter.
    pub rest: Vec<u8>,
}

/// sshsig.ParseSignature of a blob.
pub fn parse_signature(b: &[u8]) -> Result<Signature, String> {
    let (magic, rest) = b.split_at_checked(6).ok_or(SHORT)?;
    let mut w = Wire(rest);
    let version = w.u32().ok_or(SHORT)?;
    let public_key = w.string().ok_or(SHORT)?;
    let namespace = w.string().ok_or(SHORT)?;
    let _reserved = w.string().ok_or(SHORT)?;
    let hash_algorithm = w.string().ok_or(SHORT)?;
    let signature = w.string().ok_or(SHORT)?;
    if !w.0.is_empty() {
        return Err("ssh: parse error in message type 0".into());
    }
    if version != 1 {
        return Err(format!("unsupported signature version {version}: expected 1"));
    }
    if magic != b"SSHSIG" {
        return Err(format!(
            "invalid magic preamble {:?}: expected \"SSHSIG\"",
            String::from_utf8_lossy(magic)
        ));
    }
    if hash_algorithm != b"sha256" && hash_algorithm != b"sha512" {
        return Err(format!(
            "unsupported hash algorithm {:?}",
            String::from_utf8_lossy(hash_algorithm)
        ));
    }
    let mut s = Wire(signature);
    let format = s.string().ok_or(SHORT)?;
    let blob = s.string().ok_or(SHORT)?;
    let rest = s.0;
    let public_key = parse_public_key(public_key)?;
    if matches!(public_key, PublicKey::Rsa { .. }) && format != b"rsa-sha2-256" && format != b"rsa-sha2-512" {
        return Err(format!(
            "invalid signature format {:?}: expected \"rsa-sha2-256\" or \"rsa-sha2-512\"",
            String::from_utf8_lossy(format)
        ));
    }
    Ok(Signature {
        version,
        public_key,
        namespace: namespace.to_vec(),
        hash_algorithm: hash_algorithm.to_vec(),
        format: format.to_vec(),
        blob: blob.to_vec(),
        rest: rest.to_vec(),
    })
}

/// unicode.IsSpace of a rune.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// bytes.TrimSpace: leading and trailing white space, runes decoded where they are
/// valid UTF-8.
fn trim_space(mut b: &[u8]) -> &[u8] {
    loop {
        let Some(&c) = b.first() else { return b };
        let (r, w) = first_rune(b);
        if c < 0x80 && !(c as char).is_ascii_whitespace() && c != 0x0b {
            break;
        }
        if !r.is_some_and(is_space) {
            break;
        }
        b = b.get(w..).unwrap_or_default();
    }
    loop {
        let Some(&c) = b.last() else { return b };
        let (r, w) = last_rune(b);
        if c < 0x80 && !(c as char).is_ascii_whitespace() && c != 0x0b {
            break;
        }
        if !r.is_some_and(is_space) {
            break;
        }
        b = b.get(..b.len() - w).unwrap_or_default();
    }
    b
}

fn first_rune(b: &[u8]) -> (Option<char>, usize) {
    for w in 1..=4.min(b.len()) {
        if let Some(c) = b
            .get(..w)
            .and_then(|x| std::str::from_utf8(x).ok())
            .and_then(|x| x.chars().next())
        {
            return (Some(c), w);
        }
    }
    (None, 1)
}

fn last_rune(b: &[u8]) -> (Option<char>, usize) {
    for w in 1..=4.min(b.len()) {
        if let Some(c) = b
            .get(b.len() - w..)
            .and_then(|x| std::str::from_utf8(x).ok())
            .and_then(|x| x.chars().next())
        {
            return (Some(c), w);
        }
    }
    (None, 1)
}

/// parseAuthorizedKey: base64 up to the first space or tab, a public key.
fn authorized_key(input: &[u8]) -> Result<PublicKey, String> {
    let input = trim_space(input);
    let i = input
        .iter()
        .position(|&c| c == b' ' || c == b'\t')
        .unwrap_or(input.len());
    let key = crate::armor::base64(input.get(..i).unwrap_or_default()).map_err(|e| e.to_string())?;
    parse_public_key(&key)
}

/// ssh.ParseAuthorizedKey: the first line's key whose declared type is the key's own,
/// with or without options before it; the last line's error where none is.
/// `quote` is Go's strconv.Quote, for the declared type in its error.
pub fn parse_authorized_key(mut input: &[u8], quote: &dyn Fn(&[u8]) -> String) -> Result<PublicKey, String> {
    let mut last: Option<String> = None;
    while !input.is_empty() {
        let (mut line, rest) = match input.iter().position(|&c| c == b'\n') {
            Some(end) => (
                input.get(..end).unwrap_or_default(),
                input.get(end + 1..).unwrap_or_default(),
            ),
            None => (input, &[][..]),
        };
        if let Some(end) = line.iter().position(|&c| c == b'\r') {
            line = line.get(..end).unwrap_or_default();
        }
        input = rest;
        let line = trim_space(line);
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        let Some(i) = line.iter().position(|&c| c == b' ' || c == b'\t') else {
            continue;
        };
        let mismatch = |declared: &[u8], key: &PublicKey| {
            format!(
                "ssh: authorized keys key type mismatch: human-readable type {}, encoded type {:?}",
                quote(declared),
                key.kind()
            )
        };
        match authorized_key(line.get(i..).unwrap_or_default()) {
            Ok(key) if line.get(..i) == Some(key.kind().as_bytes()) => return Ok(key),
            Ok(key) => last = Some(mismatch(line.get(..i).unwrap_or_default(), &key)),
            Err(e) => last = Some(e),
        }
        // An options field first.
        let mut in_quote = false;
        let mut i = 0;
        for (at, &b) in line.iter().enumerate() {
            i = at;
            let is_end = !in_quote && (b == b' ' || b == b'\t');
            if is_end {
                break;
            }
            if b == b'"' && (at == 0 || line.get(at - 1) != Some(&b'\\')) {
                in_quote = !in_quote;
            }
        }
        while line.get(i).is_some_and(|&c| c == b' ' || c == b'\t') {
            i += 1;
        }
        if i == line.len() {
            continue;
        }
        let line = line.get(i..).unwrap_or_default();
        let Some(i) = line.iter().position(|&c| c == b' ' || c == b'\t') else {
            continue;
        };
        match authorized_key(line.get(i..).unwrap_or_default()) {
            Ok(key) if line.get(..i) == Some(key.kind().as_bytes()) => return Ok(key),
            Ok(key) => last = Some(mismatch(line.get(..i).unwrap_or_default(), &key)),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => format!("ssh: no key found; last parsing error for ignored line: {e}"),
        None => "ssh: no key found".into(),
    })
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put(out, s);
}

/// ssh.Unmarshal of an ECDSA blob: two mpints and nothing after them.
fn ecdsa_blob(blob: &[u8]) -> Result<(Int, Int), String> {
    if blob.is_empty() {
        return Err("ssh: parse error in message type 0".into());
    }
    let mut w = Wire(blob);
    let r = w.int().ok_or(SHORT)?;
    let s = w.int().ok_or(SHORT)?;
    if !w.0.is_empty() {
        return Err("ssh: parse error in message type 0".into());
    }
    Ok((r, s))
}

/// A security key's signature fields: its flags and counter, exactly.
fn sk_fields(rest: &[u8]) -> Result<(u8, u32), String> {
    if rest.is_empty() {
        return Err("ssh: parse error in message type 0".into());
    }
    let (&flags, rest) = rest.split_first().ok_or(SHORT)?;
    let (counter, rest) = rest.split_first_chunk::<4>().ok_or(SHORT)?;
    if !rest.is_empty() {
        return Err("ssh: parse error in message type 0".into());
    }
    Ok((flags, u32::from_be_bytes(*counter)))
}

fn did_not_verify() -> String {
    "ssh: signature did not verify".into()
}

/// ecdsa.Verify on a NIST curve of a digest: r and s positive.
fn ecdsa(curve: crate::key::Curve, point: &[u8], digest: &[u8], r: &Int, s: &Int) -> bool {
    !r.negative && !s.negative && crate::verify::ecdsa_aws(curve, point, digest, &r.magnitude, &s.magnitude)
}

fn sha(hash: crate::signature::Hash, data: &[u8]) -> Vec<u8> {
    use aws_lc_rs::digest;
    let alg = match hash {
        crate::signature::Hash::Sha1 => &digest::SHA1_FOR_LEGACY_USE_ONLY,
        crate::signature::Hash::Sha384 => &digest::SHA384,
        crate::signature::Hash::Sha512 => &digest::SHA512,
        _ => &digest::SHA256,
    };
    digest::digest(alg, data).as_ref().to_vec()
}

fn ed25519(public: &[u8], message: &[u8], sig: &[u8]) -> bool {
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
    public.len() == 32
        && UnparsedPublicKey::new(&ED25519, public)
            .verify(message, sig)
            .is_ok()
}

/// sshsig.Verify of `message` by `key`: the key the signature names, then the key's
/// Verify of the signed data (the preamble, namespace, hash algorithm and the message's
/// hash) against the signature.
pub fn verify(message: &[u8], sig: &Signature, key: &PublicKey) -> Result<(), String> {
    use crate::signature::Hash;
    if key.fingerprint() != sig.public_key.fingerprint() {
        return Err("public key does not match".into());
    }
    let hash = if sig.hash_algorithm == b"sha512" {
        Hash::Sha512
    } else {
        Hash::Sha256
    };
    let mut data = b"SSHSIG".to_vec();
    put_string(&mut data, &sig.namespace);
    put_string(&mut data, b"");
    put_string(&mut data, &sig.hash_algorithm);
    put_string(&mut data, &sha(hash, message));
    let format = sig.format.as_slice();
    let wrong_type = || {
        format!(
            "ssh: signature type {} for key type {}",
            String::from_utf8_lossy(format),
            key.kind()
        )
    };
    match key {
        PublicKey::Rsa { e, n } => {
            let hash = match format {
                b"rsa-sha2-256" => Hash::Sha256,
                b"rsa-sha2-512" => Hash::Sha512,
                b"ssh-rsa" => Hash::Sha1,
                _ => return Err(wrong_type()),
            };
            let digest = sha(hash, &data);
            let k = n.bit_len().div_ceil(8);
            let mut blob = sig.blob.clone();
            if blob.len() < k {
                let mut padded = vec![0u8; k - blob.len()];
                padded.extend(blob);
                blob = padded;
            }
            if let Some(e) = crate::arith::rsa_key_error(&n.magnitude, &e.magnitude) {
                return Err(e);
            }
            crate::arith::rsa_pkcs1_verify(&n.magnitude, &e.magnitude, hash, &digest, &blob)
                .then_some(())
                .ok_or_else(|| "crypto/rsa: verification error".into())
        }
        PublicKey::Dsa { p, q, g, y } => {
            if format != b"ssh-dss" {
                return Err(wrong_type());
            }
            let digest = sha(Hash::Sha1, &data);
            if sig.blob.len() != 40 {
                return Err("ssh: DSA signature parse error".into());
            }
            let (r, s) = sig.blob.split_at(20);
            // dsa.Verify over big.Ints: a negative Q refuses every r; a negative G
            // raised as Go's Exp raises it, modulo |P|.
            if q.negative {
                return Err(did_not_verify());
            }
            let g = if g.negative && !g.is_zero() {
                use num_bigint::BigUint;
                let p = BigUint::from_bytes_be(&p.magnitude);
                if p.bits() == 0 {
                    return Err(did_not_verify());
                }
                let m = BigUint::from_bytes_be(&g.magnitude) % &p;
                ((&p - m) % &p).to_bytes_be()
            } else {
                g.magnitude.clone()
            };
            crate::arith::dsa_verify(&p.magnitude, &q.magnitude, &g, &y.magnitude, &digest, r, s)
                .then_some(())
                .ok_or_else(did_not_verify)
        }
        PublicKey::Ecdsa { curve, point } => {
            if format != key.kind().as_bytes() {
                return Err(wrong_type());
            }
            let (c, hash) = match *curve {
                "nistp256" => (crate::key::Curve::P256, Hash::Sha256),
                "nistp384" => (crate::key::Curve::P384, Hash::Sha384),
                _ => (crate::key::Curve::P521, Hash::Sha512),
            };
            let digest = sha(hash, &data);
            let (r, s) = ecdsa_blob(&sig.blob)?;
            ecdsa(c, point, &digest, &r, &s)
                .then_some(())
                .ok_or_else(did_not_verify)
        }
        PublicKey::Ed25519(k) => {
            if format != key.kind().as_bytes() {
                return Err(wrong_type());
            }
            ed25519(k, &data, &sig.blob)
                .then_some(())
                .ok_or_else(did_not_verify)
        }
        PublicKey::SkEcdsa { point, application } => {
            if format != key.kind().as_bytes() {
                return Err(wrong_type());
            }
            let app = sha(Hash::Sha256, application);
            let msg = sha(Hash::Sha256, &data);
            let (r, s) = ecdsa_blob(&sig.blob)?;
            let (flags, counter) = sk_fields(&sig.rest)?;
            if flags & 0x01 == 0 {
                return Err("ssh: signature missing required user presence flag".into());
            }
            let mut blob = app;
            blob.push(flags);
            blob.extend_from_slice(&counter.to_be_bytes());
            blob.extend_from_slice(&msg);
            let digest = sha(Hash::Sha256, &blob);
            ecdsa(crate::key::Curve::P256, point, &digest, &r, &s)
                .then_some(())
                .ok_or_else(did_not_verify)
        }
        PublicKey::SkEd25519 { key: k, application } => {
            if format != key.kind().as_bytes() {
                return Err(wrong_type());
            }
            let app = sha(Hash::Sha256, application);
            let msg = sha(Hash::Sha256, &data);
            if sig.blob.is_empty() {
                return Err("ssh: parse error in message type 0".into());
            }
            let (flags, counter) = sk_fields(&sig.rest)?;
            if flags & 0x01 == 0 {
                return Err("ssh: signature missing required user presence flag".into());
            }
            let mut original = app;
            original.push(flags);
            original.extend_from_slice(&counter.to_be_bytes());
            original.extend_from_slice(&msg);
            ed25519(k, &original, &sig.blob)
                .then_some(())
                .ok_or_else(did_not_verify)
        }
    }
}
