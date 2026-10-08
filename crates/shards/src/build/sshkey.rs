//! The key files an `--ssh ID=FILE` names, read as buildx v0.37.1's agent reads them
//! (golang.org/x/crypto v0.55.0's `ssh.ParseRawPrivateKey`, then `agent.NewKeyring`'s
//! `Add`), and the agent made of them, answering a step as that keyring answers it
//! (`agent.ServeAgent`), signing with AWS-LC (D68).
//!
//! Better than buildx's: the agent is read-only, as a forwarded one is (steps cannot add,
//! remove or lock its keys); and it refuses what no longer protects anyone: DSA keys, RSA
//! keys outside 2048–8192 bits, and RSA's SHA-1 signatures (`ssh-rsa`).

use aws_lc_rs::signature::{self, KeyPair as _};
use shards_cmdline::buildflags::KeyRefused;
use shards_cmdline::go;
use zeroize::Zeroizing;

/// A key an agent made of a file holds: its public blob, as SSH writes it (RFC 4253
/// §6.6, RFC 5656 §3.1, RFC 8709 §4), and what signs with it.
// No builder runs on Windows yet: its keys are read there, and served nowhere.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Key {
    blob: Vec<u8>,
    signer: Signer,
}

impl std::fmt::Debug for Key {
    /// Its type alone: nothing of the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut w = Wire(&self.blob);
        let format = w.bytes().unwrap_or_default();
        write!(f, "Key({})", String::from_utf8_lossy(format))
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
enum Signer {
    Ed25519(signature::Ed25519KeyPair),
    Ecdsa(signature::EcdsaKeyPair, Curve),
    Rsa(aws_lc_rs::rsa::KeyPair),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Curve {
    P256,
    P384,
    P521,
}

impl Curve {
    fn name(self) -> &'static str {
        match self {
            Curve::P256 => "nistp256",
            Curve::P384 => "nistp384",
            Curve::P521 => "nistp521",
        }
    }

    /// The bytes of a scalar or a coordinate.
    fn len(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
        }
    }

    /// Signing with the hash RFC 5656 §6.2.1 pairs with the curve, `r || s` fixed.
    fn signing(self) -> &'static signature::EcdsaSigningAlgorithm {
        match self {
            Curve::P256 => &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            Curve::P384 => &signature::ECDSA_P384_SHA384_FIXED_SIGNING,
            Curve::P521 => &signature::ECDSA_P521_SHA512_FIXED_SIGNING,
        }
    }

    /// The named curve's OID (RFC 5480 §2.1.1.1), DER contents.
    fn of_oid(oid: &[u8]) -> Option<Curve> {
        match oid {
            [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07] => Some(Curve::P256),
            [0x2b, 0x81, 0x04, 0x00, 0x22] => Some(Curve::P384),
            [0x2b, 0x81, 0x04, 0x00, 0x23] => Some(Curve::P521),
            _ => None,
        }
    }
}

/// P-224's OID: a curve Go's x509 reads, and its SSH agent then refuses.
const P224: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x21];

const NOT_P_CURVE: &str = "ssh: only P-256, P-384 and P-521 EC keys are supported";

/// RSA keys shards signs with: none shorter than NIST SP 800-131A Rev. 2 (§3) allows for
/// signatures, none longer than AWS-LC takes.
const RSA_BITS: std::ops::RangeInclusive<usize> = 2048..=8192;

fn parse_err(s: impl Into<String>) -> KeyRefused {
    KeyRefused::Parse(s.into())
}

/// A key file's key, or why buildx's agent would not take it, or shards does not.
pub fn parse(file: &[u8]) -> Result<Key, KeyRefused> {
    let block = pem_decode(file).ok_or_else(|| parse_err("ssh: no key found"))?;
    if block.header("Proc-Type").is_some_and(|v| v.contains("ENCRYPTED")) {
        return Err(parse_err("ssh: this private key is passphrase protected"));
    }
    match block.kind.as_str() {
        "RSA PRIVATE KEY" => rsa_key(aws_lc_rs::rsa::KeyPair::from_der(&block.bytes)),
        "PRIVATE KEY" => pkcs8(&block.bytes),
        "EC PRIVATE KEY" => sec1(&block.bytes),
        // x/crypto still reads these; OpenSSH 10.0 removed them, their signatures being
        // SHA-1's over 160-bit groups (FIPS 186-5 no longer approves DSA).
        "DSA PRIVATE KEY" => Err(parse_err(
            "ssh: DSA keys are refused: FIPS 186-5 withdrew DSA, and OpenSSH 10.0 removed it",
        )),
        "OPENSSH PRIVATE KEY" => openssh(&block.bytes),
        other => Err(parse_err(format!(
            "ssh: unsupported key type {}",
            go::quote(other)
        ))),
    }
}

/// The keyring `keys` make, as x/crypto's keyring adds them: a key given again replaces
/// the one it equals, where it stood.
pub fn keyring(keys: Vec<Key>) -> Vec<Key> {
    let mut ring: Vec<Key> = Vec::with_capacity(keys.len());
    for key in keys {
        match ring.iter_mut().find(|k| k.blob == key.blob) {
            Some(k) => *k = key,
            None => ring.push(key),
        }
    }
    ring
}

/// SSH_AGENT_FAILURE.
#[cfg_attr(not(unix), allow(dead_code))]
const FAILURE: u8 = 5;

/// The agent's answer to `request` (a message's body, draft-miller-ssh-agent §3): its
/// keys (11), or a signature by one (13); every other request refused.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn answer(keys: &[Key], request: &[u8]) -> Vec<u8> {
    match request.first() {
        Some(11) => {
            let mut out = vec![12];
            out.extend_from_slice(&u32::try_from(keys.len()).unwrap_or(0).to_be_bytes());
            for k in keys {
                put_string(&mut out, &k.blob);
                put_string(&mut out, b"");
            }
            out
        }
        Some(13) => sign(keys, request.get(1..).unwrap_or_default()).unwrap_or_else(|| vec![FAILURE]),
        _ => vec![FAILURE],
    }
}

/// SSH_AGENTC_SIGN_REQUEST: the key's blob, the data, the flags, and nothing after.
#[cfg_attr(not(unix), allow(dead_code))]
fn sign(keys: &[Key], body: &[u8]) -> Option<Vec<u8>> {
    let mut w = Wire(body);
    let (blob, data, flags) = (w.bytes()?, w.bytes()?, w.u32()?);
    if !w.0.is_empty() {
        return None;
    }
    let key = keys.iter().find(|k| k.blob == blob)?;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let (format, sig): (&str, Vec<u8>) = match (&key.signer, flags) {
        (Signer::Ed25519(k), 0) => ("ssh-ed25519", k.sign(data).as_ref().to_vec()),
        (Signer::Ecdsa(k, curve), 0) => {
            let fixed = k.sign(&rng, data).ok()?;
            let (r, s) = fixed.as_ref().split_at_checked(curve.len())?;
            let mut rs = Vec::new();
            put_string(&mut rs, &mpint(r));
            put_string(&mut rs, &mpint(s));
            (
                match curve {
                    Curve::P256 => "ecdsa-sha2-nistp256",
                    Curve::P384 => "ecdsa-sha2-nistp384",
                    Curve::P521 => "ecdsa-sha2-nistp521",
                },
                rs,
            )
        }
        // SSH_AGENT_RSA_SHA2_256 and _512 (RFC 8332 §3.2); without either, `ssh-rsa`'s
        // SHA-1, which shards does not sign with (RFC 8332 §1, OpenSSH 8.8).
        (Signer::Rsa(k), 2 | 4) => {
            let (format, encoding): (&str, &'static dyn signature::RsaEncoding) = if flags == 2 {
                ("rsa-sha2-256", &signature::RSA_PKCS1_SHA256)
            } else {
                ("rsa-sha2-512", &signature::RSA_PKCS1_SHA512)
            };
            let mut sig = vec![0; k.public_modulus_len()];
            k.sign(encoding, &rng, data, &mut sig).ok()?;
            (format, sig)
        }
        _ => return None,
    };
    let mut wire = Vec::new();
    put_string(&mut wire, format.as_bytes());
    put_string(&mut wire, &sig);
    let mut out = vec![14];
    put_string(&mut out, &wire);
    Some(out)
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&u32::try_from(s.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(s);
}

/// An unsigned big-endian number as an mpint's contents (RFC 4251 §5): no leading zero
/// but one before a high bit; zero empty.
fn mpint(n: &[u8]) -> Vec<u8> {
    let n = strip(n);
    let mut out = Vec::with_capacity(n.len() + 1);
    if n.first().is_some_and(|b| b & 0x80 != 0) {
        out.push(0);
    }
    out.extend_from_slice(n);
    out
}

fn strip(n: &[u8]) -> &[u8] {
    let zeros = n.iter().take_while(|b| **b == 0).count();
    n.get(zeros..).unwrap_or_default()
}

fn bits(n: &[u8]) -> usize {
    let n = strip(n);
    n.first()
        .map_or(0, |b| (n.len() - 1) * 8 + (8 - b.leading_zeros() as usize))
}

/// An RSA key AWS-LC made (which checks its parts agree, RSA_check_key), or why not.
fn rsa_key(made: Result<aws_lc_rs::rsa::KeyPair, aws_lc_rs::error::KeyRejected>) -> Result<Key, KeyRefused> {
    let k = made.map_err(|e| match e.to_string().as_str() {
        "TooSmall" | "TooLarge" => rsa_size_refused(),
        why => parse_err(format!("ssh: the RSA key is not valid ({why})")),
    })?;
    // RSAPublicKey (RFC 8017 §A.1.1): n, then e.
    let public = k.public_key().as_ref();
    let (n, e) = der(public)
        .and_then(|(_, body, _)| der(body))
        .and_then(|(_, n, rest)| Some((n, der(rest)?.1)))
        .ok_or_else(|| parse_err("ssh: the RSA key is not valid (its public key)"))?;
    let mut blob = Vec::new();
    put_string(&mut blob, b"ssh-rsa");
    put_string(&mut blob, &mpint(e));
    put_string(&mut blob, &mpint(n));
    Ok(Key {
        blob,
        signer: Signer::Rsa(k),
    })
}

fn rsa_size_refused() -> KeyRefused {
    parse_err(format!(
        "ssh: an RSA key must have {} to {} bits (NIST SP 800-131A), and this one does not",
        RSA_BITS.start(),
        RSA_BITS.end()
    ))
}

fn ecdsa_key(k: signature::EcdsaKeyPair, curve: Curve) -> Key {
    let mut blob = Vec::new();
    put_string(&mut blob, format!("ecdsa-sha2-{}", curve.name()).as_bytes());
    put_string(&mut blob, curve.name().as_bytes());
    put_string(&mut blob, k.public_key().as_ref());
    Key {
        blob,
        signer: Signer::Ecdsa(k, curve),
    }
}

fn ed25519_key(k: signature::Ed25519KeyPair) -> Key {
    let mut blob = Vec::new();
    put_string(&mut blob, b"ssh-ed25519");
    put_string(&mut blob, k.public_key().as_ref());
    Key {
        blob,
        signer: Signer::Ed25519(k),
    }
}

fn invalid(what: &str, e: aws_lc_rs::error::KeyRejected) -> KeyRefused {
    parse_err(format!("ssh: the {what} key is not valid ({e})"))
}

/// One DER element (X.690 §8.1): its tag, its contents, and what follows it.
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return None;
        }
        let (bytes, rest) = rest.split_at_checked(n)?;
        (bytes.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)), rest)
    };
    let (body, rest) = rest.split_at_checked(len)?;
    Some((tag, body, rest))
}

/// An OID's contents in dotted form, as Go's `asn1.ObjectIdentifier` prints it.
fn dotted(oid: &[u8]) -> String {
    let mut arcs: Vec<u64> = Vec::new();
    let mut acc: u64 = 0;
    for b in oid {
        acc = acc.saturating_mul(128).saturating_add(u64::from(b & 0x7f));
        if b & 0x80 == 0 {
            if arcs.is_empty() {
                let first = (acc / 40).min(2);
                arcs.push(first);
                arcs.push(acc - first * 40);
            } else {
                arcs.push(acc);
            }
            acc = 0;
        }
    }
    arcs.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
}

const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
const OID_X25519: &[u8] = &[0x2b, 0x65, 0x6e];

/// PKCS#8's PrivateKeyInfo (RFC 5208 §5), by its algorithm, as Go's x509 takes it.
fn pkcs8(bytes: &[u8]) -> Result<Key, KeyRefused> {
    let malformed = || parse_err("ssh: the key file's PKCS#8 structure is malformed");
    let (0x30, info, _) = der(bytes).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let (0x02, _, rest) = der(info).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let (0x30, alg, _) = der(rest).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let (0x06, oid, params) = der(alg).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    match oid {
        OID_RSA => rsa_key(aws_lc_rs::rsa::KeyPair::from_pkcs8(bytes)),
        OID_EC => {
            let named = match der(params) {
                Some((0x06, curve, _)) => curve,
                _ => &[],
            };
            if named == P224 {
                return Err(KeyRefused::Add(NOT_P_CURVE.into()));
            }
            let curve = Curve::of_oid(named).ok_or_else(|| {
                parse_err(
                    "x509: failed to parse EC private key embedded in PKCS#8: x509: unknown elliptic curve",
                )
            })?;
            signature::EcdsaKeyPair::from_pkcs8(curve.signing(), bytes)
                .map(|k| ecdsa_key(k, curve))
                .map_err(|e| invalid("ECDSA", e))
        }
        OID_ED25519 => signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(bytes)
            .map(ed25519_key)
            .map_err(|e| invalid("Ed25519", e)),
        OID_X25519 => Err(KeyRefused::Add(
            "ssh: unsupported key type *ecdh.PrivateKey".into(),
        )),
        _ => Err(parse_err(format!(
            "x509: PKCS#8 wrapping contained private key with unknown algorithm: {}",
            dotted(oid)
        ))),
    }
}

/// SEC 1's ECPrivateKey (RFC 5915 §3), on the curve its parameters name.
fn sec1(bytes: &[u8]) -> Result<Key, KeyRefused> {
    let malformed = || parse_err("ssh: the key file's EC private key structure is malformed");
    let (0x30, body, _) = der(bytes).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let (0x02, _, rest) = der(body).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let (0x04, _, rest) = der(rest).ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let named = match der(rest) {
        Some((0xa0, params, _)) => match der(params) {
            Some((0x06, oid, _)) => oid,
            _ => &[],
        },
        _ => &[],
    };
    if named == P224 {
        return Err(KeyRefused::Add(NOT_P_CURVE.into()));
    }
    let curve = Curve::of_oid(named).ok_or_else(|| parse_err("x509: unknown elliptic curve"))?;
    signature::EcdsaKeyPair::from_private_key_der(curve.signing(), bytes)
        .map(|k| ecdsa_key(k, curve))
        .map_err(|e| invalid("ECDSA", e))
}

/// SSH wire fields (RFC 4251 §5), failing as x/crypto's `Unmarshal` fails.
struct Wire<'a>(&'a [u8]);

impl<'a> Wire<'a> {
    fn u32(&mut self) -> Option<u32> {
        let (n, rest) = self.0.split_first_chunk::<4>()?;
        self.0 = rest;
        Some(u32::from_be_bytes(*n))
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u32()?).ok()?;
        let (s, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(s)
    }
}

/// x/crypto's `Unmarshal` of a struct of `fields` from `data`: each field a `u32`, a
/// `string`, `[]byte` or `*big.Int`, the last the rest; its errors in its words.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    U32,
    Str,
    Bytes,
    Int,
}

fn unmarshal<'a>(
    data: &'a [u8],
    of: &str,
    fields: &[(&str, Field)],
) -> Result<(Vec<&'a [u8]>, &'a [u8]), String> {
    const SHORT: &str = "ssh: short read";
    if data.is_empty() {
        return Err("ssh: parse error in message type 0".into());
    }
    let mut w = Wire(data);
    let mut out = Vec::with_capacity(fields.len());
    for (name, kind) in fields {
        match kind {
            Field::U32 => {
                let at = w.0;
                w.u32().ok_or(SHORT)?;
                out.push(at.get(..4).unwrap_or_default());
            }
            Field::Str => out.push(
                w.bytes()
                    .ok_or_else(|| format!("ssh: unmarshal error for field {name} of type {of}"))?,
            ),
            Field::Bytes | Field::Int => out.push(w.bytes().ok_or(SHORT)?),
        }
    }
    Ok((out, w.0))
}

fn be32(b: &[u8]) -> u32 {
    b.first_chunk::<4>().map_or(0, |b| u32::from_be_bytes(*b))
}

/// `checkOpenSSHKeyPadding`: 1, 2, 3, ...
fn padding(pad: &[u8]) -> Result<(), KeyRefused> {
    if pad.iter().enumerate().all(|(i, b)| usize::from(*b) == i + 1) {
        Ok(())
    } else {
        Err(parse_err("ssh: padding not as expected"))
    }
}

/// The OpenSSH key format (PROTOCOL.key), unencrypted, one key, as x/crypto's
/// `parseOpenSSHPrivateKey` reads it.
fn openssh(bytes: &[u8]) -> Result<Key, KeyRefused> {
    use Field::{Bytes, Int, Str, U32};
    let rest = bytes
        .strip_prefix(b"openssh-key-v1\0")
        .ok_or_else(|| parse_err("ssh: invalid openssh private key format"))?;
    let (w, _) = unmarshal(
        rest,
        "openSSHEncryptedPrivateKey",
        &[
            ("CipherName", Str),
            ("KdfName", Str),
            ("KdfOpts", Str),
            ("NumKeys", U32),
            ("PubKey", Bytes),
            ("PrivKeyBlock", Bytes),
        ],
    )
    .map_err(parse_err)?;
    let [cipher, kdf, kdf_opts, num, _, block] = w.as_slice() else {
        return Err(parse_err("ssh: malformed OpenSSH key"));
    };
    if be32(num) != 1 {
        return Err(parse_err("ssh: multi-key files are not supported"));
    }
    if *kdf != b"none" || *cipher != b"none" {
        return Err(parse_err("ssh: this private key is passphrase protected"));
    }
    if !kdf_opts.is_empty() {
        return Err(parse_err("ssh: invalid openssh private key"));
    }
    let malformed = || parse_err("ssh: malformed OpenSSH key");
    let (pk1, key) = unmarshal(
        block,
        "openSSHPrivateKey",
        &[("Check1", U32), ("Check2", U32), ("Keytype", Str)],
    )
    .map_err(|_| malformed())?;
    let [check1, check2, keytype] = pk1.as_slice() else {
        return Err(malformed());
    };
    if check1 != check2 {
        return Err(malformed());
    }
    match *keytype {
        b"ssh-rsa" => {
            let (f, pad) = unmarshal(
                key,
                "openSSHRSAPrivateKey",
                &[
                    ("N", Int),
                    ("E", Int),
                    ("D", Int),
                    ("Iqmp", Int),
                    ("P", Int),
                    ("Q", Int),
                    ("Comment", Str),
                ],
            )
            .map_err(parse_err)?;
            let [n, e, d, iqmp, p, q, _] = f.as_slice() else {
                return Err(malformed());
            };
            padding(pad)?;
            if bits(n) > 16384 {
                return Err(parse_err("ssh: rsa modulus too large"));
            }
            if bits(p) > 8192 || bits(q) > 8192 {
                return Err(parse_err("ssh: rsa prime too large"));
            }
            if bits(e) > 24 {
                return Err(parse_err("ssh: exponent too large"));
            }
            let exponent = strip(e).iter().fold(0u32, |a, b| (a << 8) | u32::from(*b));
            if exponent < 3 || exponent & 1 == 0 {
                return Err(parse_err("ssh: incorrect exponent"));
            }
            if [n, d, iqmp, p, q]
                .iter()
                .any(|v| v.first().is_some_and(|b| b & 0x80 != 0))
            {
                return Err(malformed());
            }
            if !RSA_BITS.contains(&bits(n)) {
                return Err(rsa_size_refused());
            }
            // PROTOCOL.key carries no CRT exponents; AWS-LC takes them, and checks them.
            let (dp, dq) = (crt_exponent(d, p), crt_exponent(d, q));
            let (Some(dp), Some(dq)) = (dp, dq) else {
                return Err(parse_err("ssh: the RSA key is not valid (its primes are not)"));
            };
            rsa_key(aws_lc_rs::rsa::KeyPair::from_components(
                &aws_lc_rs::rsa::KeyPairComponents {
                    public_key: aws_lc_rs::rsa::PublicKeyComponents {
                        n: strip(n),
                        e: strip(e),
                    },
                    d: *d,
                    p: *p,
                    q: *q,
                    dP: dp.as_slice(),
                    dQ: dq.as_slice(),
                    qInv: *iqmp,
                },
            ))
        }
        b"ssh-ed25519" => {
            let (f, pad) = unmarshal(
                key,
                "openSSHEd25519PrivateKey",
                &[("Pub", Bytes), ("Priv", Bytes), ("Comment", Str)],
            )
            .map_err(parse_err)?;
            let [_, private, _] = f.as_slice() else {
                return Err(malformed());
            };
            let Some((seed, public)) = private.split_at_checked(32).filter(|(_, p)| p.len() == 32) else {
                return Err(parse_err("ssh: private key unexpected length"));
            };
            padding(pad)?;
            // Its public key is the one its private key holds, as Go's ed25519 takes it.
            signature::Ed25519KeyPair::from_seed_and_public_key(seed, public)
                .map(ed25519_key)
                .map_err(|e| invalid("Ed25519", e))
        }
        b"ecdsa-sha2-nistp256" | b"ecdsa-sha2-nistp384" | b"ecdsa-sha2-nistp521" => {
            let (f, pad) = unmarshal(
                key,
                "openSSHECDSAPrivateKey",
                &[("Curve", Str), ("Pub", Bytes), ("D", Int), ("Comment", Str)],
            )
            .map_err(parse_err)?;
            let [name, public, scalar, _] = f.as_slice() else {
                return Err(malformed());
            };
            padding(pad)?;
            let curve = match *name {
                b"nistp256" => Curve::P256,
                b"nistp384" => Curve::P384,
                b"nistp521" => Curve::P521,
                other => {
                    return Err(parse_err(format!(
                        "ssh: unhandled elliptic curve: {}",
                        String::from_utf8_lossy(other)
                    )));
                }
            };
            if public.len() != 1 + 2 * curve.len() || public.first() != Some(&4) {
                return Err(parse_err("ssh: failed to unmarshal public key"));
            }
            let d = strip(scalar);
            if d.len() > curve.len() || scalar.first().is_some_and(|b| b & 0x80 != 0) {
                return Err(parse_err("ssh: scalar is out of range"));
            }
            let mut fixed = Zeroizing::new(vec![0u8; curve.len()]);
            if let Some(tail) = fixed.get_mut(curve.len() - d.len()..) {
                tail.copy_from_slice(d);
            }
            signature::EcdsaKeyPair::from_private_key_and_public_key(curve.signing(), &fixed, public)
                .map(|k| ecdsa_key(k, curve))
                .map_err(|_| parse_err("ssh: public key does not match private key"))
        }
        _ => Err(parse_err("ssh: unhandled key type")),
    }
}

/// `d mod (p - 1)`, for an odd prime `p`: bit by bit, each step the same work whatever
/// `d`'s bits are, so that its time tells nothing of them. None if `p` is even or under 3.
fn crt_exponent(d: &[u8], p: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let p = strip(p);
    if p.last().is_none_or(|b| b & 1 == 0) || bits(p) < 2 {
        return None;
    }
    // m = p - 1 (p odd: no borrow), as little-endian 64-bit limbs, one spare.
    let limbs = p.len().div_ceil(8) + 1;
    let mut m = vec![0u64; limbs];
    for (i, b) in p.iter().rev().enumerate() {
        if let Some(l) = m.get_mut(i / 8) {
            *l |= u64::from(*b) << ((i % 8) * 8);
        }
    }
    if let Some(l) = m.first_mut() {
        *l &= !1;
    }
    let mut r = Zeroizing::new(vec![0u64; limbs]);
    let mut t = Zeroizing::new(vec![0u64; limbs]);
    for byte in d {
        for bit in (0..8).rev() {
            // r = 2r + bit: under 2m, as r was under m.
            let mut carry = u64::from(byte >> bit & 1);
            for l in r.iter_mut() {
                let next = *l >> 63;
                *l = *l << 1 | carry;
                carry = next;
            }
            // t = r - m; kept if it did not borrow.
            let mut borrow = 0u64;
            for ((tl, rl), ml) in t.iter_mut().zip(r.iter()).zip(m.iter()) {
                let (a, b1) = rl.overflowing_sub(*ml);
                let (a, b2) = a.overflowing_sub(borrow);
                *tl = a;
                borrow = u64::from(b1 | b2);
            }
            let keep = borrow.wrapping_sub(1); // all ones where no borrow
            for (rl, tl) in r.iter_mut().zip(t.iter()) {
                *rl = (*tl & keep) | (*rl & !keep);
            }
        }
    }
    let mut out = Zeroizing::new(vec![0u8; p.len()]);
    let len = out.len();
    for (i, o) in out.iter_mut().enumerate() {
        let at = len - 1 - i;
        *o = r.get(at / 8).map_or(0, |l| (l >> ((at % 8) * 8)) as u8);
    }
    Some(out)
}

/// A PEM block (RFC 7468) as Go's `pem.Decode` finds the first: its type, its headers
/// (RFC 1421's, the last of a name kept), its bytes.
struct Block {
    kind: String,
    headers: Vec<(String, String)>,
    bytes: Zeroizing<Vec<u8>>,
}

impl Block {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Go's `getLine`: up to a newline, trailing spaces, tabs and carriage returns trimmed;
/// and what follows it.
fn get_line(data: &[u8]) -> (&[u8], &[u8]) {
    let (line, rest) = match data.iter().position(|b| *b == b'\n') {
        Some(i) => (
            data.get(..i).unwrap_or_default(),
            data.get(i + 1..).unwrap_or_default(),
        ),
        None => (data, &[][..]),
    };
    let keep = line.len()
        - line
            .iter()
            .rev()
            .take_while(|b| matches!(b, b' ' | b'\t' | b'\r'))
            .count();
    (line.get(..keep).unwrap_or_default(), rest)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn trim(s: &[u8]) -> &[u8] {
    s.trim_ascii()
}

/// Go 1.26's `encoding/pem.Decode`, its first block alone.
fn pem_decode(data: &[u8]) -> Option<Block> {
    use base64::Engine as _;
    const START: &[u8] = b"\n-----BEGIN ";
    const END: &[u8] = b"\n-----END ";
    const EOL: &[u8] = b"-----";
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireCanonical),
    );
    let start1 = START.get(1..).unwrap_or_default();
    let end1 = END.get(1..).unwrap_or_default();
    let mut rest = data;
    loop {
        if rest.starts_with(start1) {
            rest = rest.get(start1.len()..).unwrap_or_default();
        } else {
            let i = find(rest, START)?;
            rest = rest.get(i + START.len()..).unwrap_or_default();
        }
        let (type_line, after) = get_line(rest);
        rest = after;
        let Some(kind) = type_line.strip_suffix(EOL) else {
            continue;
        };
        let mut headers = Vec::new();
        loop {
            if rest.is_empty() {
                return None;
            }
            let (line, next) = get_line(rest);
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                break;
            };
            let (k, v) = (
                line.get(..colon).unwrap_or_default(),
                line.get(colon + 1..).unwrap_or_default(),
            );
            headers.push((
                String::from_utf8_lossy(trim(k)).into_owned(),
                String::from_utf8_lossy(trim(v)).into_owned(),
            ));
            rest = next;
        }
        let (end_index, trailer_index) = if headers.is_empty() && rest.starts_with(end1) {
            (0, end1.len())
        } else {
            match find(rest, END) {
                Some(i) => (i, i + END.len()),
                None => continue,
            }
        };
        let trailer = rest.get(trailer_index..).unwrap_or_default();
        let trailer_len = kind.len() + EOL.len();
        let Some((end_line, rest_of_end)) = trailer.split_at_checked(trailer_len) else {
            continue;
        };
        if !end_line.starts_with(kind) || !end_line.ends_with(EOL) {
            continue;
        }
        if !get_line(rest_of_end).0.is_empty() {
            continue;
        }
        // Spaces and tabs removed, as Go's removeSpacesAndTabs; and line breaks, which
        // Go's base64 decoder skips.
        let body: Zeroizing<Vec<u8>> = Zeroizing::new(
            rest.get(..end_index)
                .unwrap_or_default()
                .iter()
                .copied()
                .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                .collect(),
        );
        let Ok(bytes) = engine.decode(&*body) else {
            continue;
        };
        return Some(Block {
            kind: String::from_utf8_lossy(kind).into_owned(),
            headers,
            bytes: Zeroizing::new(bytes),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Exchange {
        request: String,
        answer: String,
        #[serde(default)]
        randomized: bool,
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        files: Vec<String>,
        #[serde(default)]
        error: String,
        #[serde(default)]
        stage: String,
        #[serde(default)]
        exchanges: Vec<Exchange>,
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap(), 16).unwrap())
            .collect()
    }

    /// An ECDSA answer: its format, and its signature verified against the key's blob.
    fn verify_ecdsa(request: &[u8], answer: &[u8], go: &[u8]) {
        let mut w = Wire(&request[1..]);
        let (blob, data) = (w.bytes().unwrap(), w.bytes().unwrap());
        let mut kb = Wire(blob);
        let (_, name, point) = (kb.bytes().unwrap(), kb.bytes().unwrap(), kb.bytes().unwrap());
        let (len, alg): (usize, &'static signature::EcdsaVerificationAlgorithm) = match name {
            b"nistp256" => (32, &signature::ECDSA_P256_SHA256_FIXED),
            b"nistp384" => (48, &signature::ECDSA_P384_SHA384_FIXED),
            _ => (66, &signature::ECDSA_P521_SHA512_FIXED),
        };
        for a in [answer, go] {
            assert_eq!(a[0], 14);
            let mut aw = Wire(&a[1..]);
            let mut sig = Wire(aw.bytes().unwrap());
            let (format, rs) = (sig.bytes().unwrap(), sig.bytes().unwrap());
            assert_eq!(format, [b"ecdsa-sha2-", name].concat());
            let mut rsw = Wire(rs);
            let mut fixed = Vec::new();
            for n in [rsw.bytes().unwrap(), rsw.bytes().unwrap()] {
                let n = strip(n);
                fixed.extend(std::iter::repeat_n(0, len - n.len()));
                fixed.extend_from_slice(n);
            }
            signature::UnparsedPublicKey::new(alg, point)
                .verify(data, &fixed)
                .unwrap();
        }
    }

    /// buildx's agent, case by case (scripts/sshkey/generate): the keys it takes and
    /// how it answers, byte for byte, but where shards does better, each named here.
    #[test]
    fn key_files_are_served_as_buildx_serves_them() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("testdata/sshkey.json")).unwrap();
        assert!(cases.len() >= 30);
        // Keys buildx takes and shards refuses, in shards' words.
        let refused = [
            ("rsa1024-openssh", "ssh: an RSA key must have 2048 to 8192 bits"),
            ("dsa-pem", "ssh: DSA keys are refused"),
        ];
        for case in &cases {
            let mut keys = Vec::new();
            let mut error = None;
            for f in &case.files {
                match parse(f.as_bytes()) {
                    Ok(k) => keys.push(k),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            if let Some((_, ours)) = refused.iter().find(|(n, _)| *n == case.name) {
                assert!(case.error.is_empty(), "{}: buildx took it", case.name);
                match error {
                    Some(KeyRefused::Parse(e)) => assert!(e.starts_with(ours), "{}: {e}", case.name),
                    _ => panic!("{}: not refused", case.name),
                }
                continue;
            }
            match error {
                Some(KeyRefused::Parse(e)) => {
                    assert_eq!(
                        (case.stage.as_str(), e.as_str()),
                        ("parse", case.error.as_str()),
                        "{}",
                        case.name
                    )
                }
                Some(KeyRefused::Add(e)) => {
                    assert_eq!(
                        (case.stage.as_str(), e.as_str()),
                        ("add", case.error.as_str()),
                        "{}",
                        case.name
                    )
                }
                None => assert!(
                    case.error.is_empty(),
                    "{}: buildx refused: {}",
                    case.name,
                    case.error
                ),
            }
            let ring = keyring(keys);
            for x in &case.exchanges {
                let (request, go) = (hex(&x.request), hex(&x.answer));
                let ours = answer(&ring, &request);
                // ssh-rsa's SHA-1: buildx signs, shards does not.
                let sha1 = request[0] == 13 && {
                    let mut w = Wire(&request[1..]);
                    let blob = w.bytes().unwrap_or_default();
                    let _ = w.bytes();
                    blob.starts_with(b"\0\0\0\x07ssh-rsa") && w.u32() == Some(0)
                };
                if sha1 {
                    assert_eq!(go[0], 14, "{}", case.name);
                    assert_eq!(ours, [FAILURE], "{}", case.name);
                } else if x.randomized {
                    verify_ecdsa(&request, &ours, &go);
                } else {
                    assert_eq!(ours, go, "{}: {}", case.name, x.request);
                }
            }
        }
    }

    /// The agent is read-only, as a forwarded one is: adding, removing, locking and
    /// extensions are refused, where x/crypto's keyring would do them.
    #[test]
    fn the_key_agent_is_read_only() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("testdata/sshkey.json")).unwrap();
        let file = &cases.iter().find(|c| c.name == "ed25519-openssh").unwrap().files[0];
        let ring = keyring(vec![parse(file.as_bytes()).unwrap()]);
        for request in [
            &[17u8][..],
            &[18],
            &[19],
            &[22, 0, 0, 0, 1, b'x'],
            &[25],
            &[27],
            &[1],
            &[],
        ] {
            assert_eq!(answer(&ring, request), [FAILURE], "{request:?}");
        }
        assert_eq!(answer(&ring, &[11])[..5], [12, 0, 0, 0, 1]);
    }

    /// `d mod (p - 1)` against numbers worked by hand and against a slow reduction.
    #[test]
    fn crt_exponents_are_the_remainders() {
        assert_eq!(crt_exponent(&[100], &[7]).unwrap().as_slice(), [4]); // 100 mod 6
        assert_eq!(crt_exponent(&[0, 5], &[0, 7]).unwrap().as_slice(), [5]);
        assert!(crt_exponent(&[5], &[8]).is_none());
        // A long d and a many-limb p, against u128 arithmetic.
        let d: u128 = 0xfedc_ba98_7654_3210_0123_4567_89ab_cdef;
        let p: u128 = 0x0001_0000_0000_0000_0000_0000_0000_0061;
        let got = crt_exponent(&d.to_be_bytes(), &p.to_be_bytes()).unwrap();
        let want = d % (p - 1);
        let mut padded = [0u8; 16];
        padded[16 - got.len()..].copy_from_slice(&got);
        assert_eq!(u128::from_be_bytes(padded), want);
    }
}
