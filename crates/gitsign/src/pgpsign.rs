//! What buildx v0.37.1's policy builtins verify, as BuildKit's pgpsign and gitsign
//! verify it over go-crypto v1.4.1: armored key rings read block after block
//! (ReadAllArmoredKeyRings), a detached signature checked as CheckDetachedSignature
//! checks it with the signer's key, its identity and its bindings
//! (checkMessageSignatureDetails), BuildKit's rules over that
//! (VerifyArmoredDetachedSignature), a signature over a digest made elsewhere
//! (VerifySignatureWithDigest), and a Git object's signature, OpenPGP or SSH
//! (gitsign.VerifySignature).

use zeroize::Zeroizing;

use crate::key::{PublicKey, RSA, RSA_ENCRYPT_ONLY, RSA_SIGN_ONLY};
use crate::keyring::{self, Entity, KEY_REVOCATION};
use crate::signature::{Hash, Signature};
use crate::verify::{self, Hasher, Signed};
use crate::{Error, Packet, Reader, armor, pem, ssh};

/// How the caller writes what Go's fmt writes: `%q` of bytes (strconv.Quote), and `%v`
/// of a time.Time in this process's zone (Time.String).
pub struct Formats<'a> {
    pub quote: &'a dyn Fn(&[u8]) -> String,
    pub time: &'a dyn Fn(i64) -> String,
}

impl std::fmt::Debug for Formats<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Formats")
    }
}

const PUBLIC_KEY_BLOCK: &str = "PGP PUBLIC KEY BLOCK";
const PRIVATE_KEY_BLOCK: &str = "PGP PRIVATE KEY BLOCK";

/// ReadAllArmoredKeyRings: each armored block in turn, each a public or private key
/// block (its type checked before its body is read), each read as a key ring; the next
/// block where armor.Decode's reader left off.
pub fn read_all_armored_key_rings(data: &[u8]) -> Result<Vec<Entity>, String> {
    let mut entities = Vec::new();
    let mut at = 0;
    loop {
        let rest = data.get(at..).unwrap_or_default();
        let block = match armor::decode(rest) {
            Ok(b) => b,
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("failed to decode armored public key: {e}")),
        };
        if block.kind != PUBLIC_KEY_BLOCK && block.kind != PRIVATE_KEY_BLOCK {
            return Err(format!(
                "expected public or private key block, got: {}",
                block.kind
            ));
        }
        // The body's errors come as the key ring reads it.
        let (body, taken) = block.read_body();
        let body = body.map_err(|e| format!("failed to read armored public key: {e}"))?;
        at += taken;
        let ring =
            keyring::read_key_ring(&body).map_err(|e| format!("failed to read armored public key: {e}"))?;
        entities.extend(ring);
        if taken == 0 {
            break;
        }
    }
    if entities.is_empty() {
        return Err("failed to read armored public key: no armored data found".into());
    }
    Ok(entities)
}

/// A key of an entity, as KeysById gives it: the entity, which of its keys (None its
/// primary key), that key's self-signature and its revocations.
struct Key<'e> {
    entity: &'e Entity,
    subkey: Option<usize>,
    public: &'e PublicKey,
    self_signature: Option<&'e Signature>,
    revocations: &'e [Signature],
}

/// KeysByIdUsage(id, KeyFlagSign): the keys of that ID whose self-signature lets them
/// sign.
fn signing_keys(entities: &[Entity], id: u64) -> Vec<Key<'_>> {
    let mut keys = Vec::new();
    for e in entities {
        if e.primary.key_id == id {
            keys.push(Key {
                entity: e,
                subkey: None,
                public: &e.primary,
                self_signature: e.primary_self_signature().0,
                revocations: &e.revocations,
            });
        }
        for (i, s) in e.subkeys.iter().enumerate() {
            if s.key.key_id == id {
                keys.push(Key {
                    entity: e,
                    subkey: Some(i),
                    public: &s.key,
                    self_signature: Some(&s.sig),
                    revocations: &s.revocations,
                });
            }
        }
    }
    keys.retain(|k| {
        k.self_signature
            .is_some_and(|s| s.flags_valid && s.flags & 0x02 != 0)
    });
    keys
}

/// The signed data as a signature of `sig_type` hashes it: as it is (0x00), or as
/// canonical text (0x01), each lone LF written CRLF, as NewCanonicalTextHash writes it.
fn hash_signed(h: &mut Hasher, sig_type: u8, signed: &[u8]) -> Result<(), Error> {
    match sig_type {
        0x00 => h.update(signed),
        0x01 => {
            let mut out = Vec::with_capacity(signed.len() + signed.len() / 32);
            let mut after_cr = false;
            for &c in signed {
                if after_cr {
                    after_cr = false;
                    out.push(c);
                    continue;
                }
                match c {
                    b'\r' => {
                        after_cr = true;
                        out.push(c);
                    }
                    b'\n' => out.extend_from_slice(b"\r\n"),
                    _ => out.push(c),
                }
            }
            h.update(&out);
        }
        t => return Err(Error::Unsupported(format!("unsupported signature type: {t}"))),
    }
    Ok(())
}

/// CheckDetachedSignature over `body` (the signature's packets), at `now`
/// (config.Now()): the first signature packet whose issuer has a signing key here,
/// verified by each such key in turn, as go-crypto verifies them (the same hash, each
/// try adding the hash suffix again), and the signer's details checked.
fn check_detached_signature<'e>(
    entities: &'e [Entity],
    signed: &[u8],
    body: &[u8],
    now: u32,
) -> Result<&'e Entity, Error> {
    let mut reader = Reader::new(body);
    let (sig, keys) = loop {
        let sig = match reader.next_packet()? {
            None => return Err(Error::Fixed("openpgp: signature made by unknown entity")),
            Some(Packet::Signature(s)) => s,
            Some(_) => return Err(Error::Structural("non signature packet found".into())),
        };
        let Some(id) = sig.issuer_key_id else {
            return Err(Error::Structural("signature doesn't have an issuer".into()));
        };
        let keys = signing_keys(entities, id);
        if !keys.is_empty() {
            break (sig, keys);
        }
    };
    let mut h = Hasher::new(&sig)?;
    hash_signed(&mut h, sig.sig_type, signed)?;
    let mut last = Error::Fixed("openpgp: signature made by unknown entity");
    for key in &keys {
        match verify::verify_signature(key.public, Signed::Open(h.clone()), &sig) {
            Ok(()) => {
                check_message_signature_details(key, &sig, now)?;
                return Ok(key.entity);
            }
            Err(e) => last = e,
        }
        // VerifySignature wrote the suffix into the hash the next key is tried with.
        h.update(&sig.hash_suffix);
    }
    Err(last)
}

/// checkMessageSignatureDetails, in its order: unknown critical notations; the primary
/// key, the signing subkey or the primary identity revoked; the primary key or the
/// subkey expired; any of the signatures expired. Where go-crypto would dereference a
/// missing self-signature, an error says so (D103).
fn check_message_signature_details(key: &Key<'_>, sig: &Signature, now: u32) -> Result<(), Error> {
    let missing = || Error::Structural("the signing key has no self-signature".into());
    let (primary_self, primary_identity) = key.entity.primary_self_signature();
    let primary_self = primary_self.ok_or_else(missing)?;
    let mut sigs: Vec<&Signature> = vec![sig, primary_self];
    if key.subkey.is_some() {
        let binding = key.self_signature.ok_or_else(missing)?;
        sigs.push(binding);
        sigs.push(binding.embedded.as_deref().ok_or_else(missing)?);
    }
    for s in &sigs {
        if let Some(n) = s.notations.iter().find(|n| n.critical) {
            return Err(Error::Signature(format!("unknown critical notation: {}", n.name)));
        }
    }
    if keyring::revoked(&key.entity.revocations, now)
        || (key.subkey.is_some() && keyring::revoked(key.revocations, now))
        || primary_identity.is_some_and(|i| keyring::revoked(&i.revocations, now))
    {
        return Err(Error::Fixed("openpgp: signature made by revoked key"));
    }
    if keyring::key_expired(&key.entity.primary, primary_self, now) {
        return Err(Error::Fixed("openpgp: key expired"));
    }
    if key.subkey.is_some()
        && let Some(binding) = key.self_signature
        && keyring::key_expired(key.public, binding, now)
    {
        return Err(Error::Fixed("openpgp: key expired"));
    }
    if sigs.iter().any(|s| keyring::sig_expired(s, now)) {
        return Err(Error::Fixed("openpgp: signature expired"));
    }
    Ok(())
}

/// crypto.Hash's String.
pub fn hash_name(h: Option<Hash>) -> &'static str {
    match h {
        Some(Hash::Sha1) => "SHA-1",
        Some(Hash::Sha224) => "SHA-224",
        Some(Hash::Sha256) => "SHA-256",
        Some(Hash::Sha384) => "SHA-384",
        Some(Hash::Sha512) => "SHA-512",
        Some(Hash::Sha3_256) => "SHA3-256",
        Some(Hash::Sha3_512) => "SHA3-512",
        None => "unknown hash value 0",
    }
}

/// checkEntityRevocation: any revocation of the primary key, at any time.
fn entity_revocation(e: &Entity) -> Result<(), String> {
    for r in &e.revocations {
        if r.sig_type != KEY_REVOCATION || verify::revocation_signature(&e.primary, r).is_err() {
            continue;
        }
        return Err(match &r.revocation_reason {
            Some((_, text)) if !text.is_empty() => format!("key revoked: {text}"),
            _ => "key revoked".into(),
        });
    }
    Ok(())
}

/// A primary RSA key's modulus in bits.
fn rsa_bits(pk: &PublicKey) -> Option<usize> {
    match (&pk.material, pk.algo) {
        (crate::key::Material::Rsa { n, .. }, RSA | RSA_SIGN_ONLY | RSA_ENCRYPT_ONLY) => {
            let i = n.bytes.iter().position(|x| *x != 0).unwrap_or(n.bytes.len());
            Some(n.bytes.get(i).map_or(0, |top| {
                (n.bytes.len() - i - 1) * 8 + (8 - top.leading_zeros() as usize)
            }))
        }
        _ => None,
    }
}

/// VerifyArmoredDetachedSignature with no policy: the signature and the key rings read,
/// the signature's hash and algorithm within BuildKit's (SHA-256, -384, -512; RSA, ECDSA,
/// EdDSA), checked at its own creation time, then its signer neither revoked nor of an
/// RSA key under 2048 bits, and the signature made no later than five minutes from `now`.
pub fn verify_armored_detached_signature(
    signed: &[u8],
    signature: &[u8],
    keys: &[u8],
    now: i64,
    formats: &Formats<'_>,
) -> Result<(), String> {
    let (sig, body) = crate::parse_armored_detached_signature(signature)?;
    let body = Zeroizing::new(body);
    let entities = read_all_armored_key_rings(keys)?;
    if !matches!(sig.hash, Some(Hash::Sha256 | Hash::Sha384 | Hash::Sha512)) {
        return Err(format!("rejecting weak/unknown hash: {}", hash_name(sig.hash)));
    }
    if !matches!(sig.pubkey_algo, 22 | 19 | 1 | 3) {
        return Err(format!(
            "rejecting unsupported pubkey algorithm: {}",
            sig.pubkey_algo
        ));
    }
    let created = sig.creation_time.unwrap_or(0);
    let signer =
        check_detached_signature(&entities, signed, &body, created).map_err(|e| match sig.issuer_key_id {
            Some(id) => format!("signature by {id:X}: {e}"),
            None => e.to_string(),
        })?;
    entity_revocation(signer)?;
    if let Some(bits) = rsa_bits(&signer.primary).filter(|b| *b < 2048) {
        return Err(format!("RSA key too short: {bits} bits"));
    }
    if i64::from(created) > now.saturating_add(5 * 60) {
        return Err(format!(
            "signature creation time is in the future: {}",
            (formats.time)(i64::from(created))
        ));
    }
    Ok(())
}

/// VerifySignatureWithDigest: `digest` (of the signed data and the signature's hash
/// suffix, `algorithm` its name) checked against every key of every entity. BuildKit
/// checks no more; shards also refuses a key whose primary key is revoked, or a revoked
/// subkey (D103).
pub fn verify_signature_with_digest(
    sig: &Signature,
    entities: &[Entity],
    algorithm: &str,
    hex: &str,
) -> Result<(), String> {
    let expected = match sig.hash {
        Some(Hash::Sha256) => "sha256",
        Some(Hash::Sha384) => "sha384",
        Some(Hash::Sha512) => "sha512",
        h => return Err(format!("unsupported signature hash algorithm {}", hash_name(h))),
    };
    if algorithm != expected {
        return Err(format!("digest algorithm mismatch: {algorithm} != {expected}"));
    }
    let sum = decode_hex(hex).ok_or_else(|| format!("invalid digest hex: {}", hex_error(hex)))?;
    let size = match sig.hash {
        Some(Hash::Sha384) => 48,
        Some(Hash::Sha512) => 64,
        _ => 32,
    };
    if sum.len() != size {
        return Err(format!(
            "digest size mismatch: got {}, expected {size}",
            sum.len()
        ));
    }
    for e in entities {
        if entity_revocation(e).is_err() {
            continue;
        }
        if verify::verify_signature(&e.primary, Signed::Digest(sum.clone()), sig).is_ok() {
            return Ok(());
        }
        for s in &e.subkeys {
            if !s.revocations.is_empty() {
                continue;
            }
            if verify::verify_signature(&s.key, Signed::Digest(sum.clone()), sig).is_ok() {
                return Ok(());
            }
        }
    }
    Err("failed to verify signature with checksum digest".into())
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let b = s.as_bytes();
    (0..b.len())
        .step_by(2)
        .map(|i| {
            let d = |c: u8| (c as char).to_digit(16);
            Some((d(*b.get(i)?)? * 16 + d(*b.get(i + 1)?)?) as u8)
        })
        .collect()
}

/// hex.DecodeString's error for `s`: the first bad byte of each pair in turn, then of
/// an odd last byte, else the odd length.
fn hex_error(s: &str) -> String {
    let b = s.as_bytes();
    let bad = |c: u8| format!("encoding/hex: invalid byte: {}", sharp_u(c));
    let mut j = 1;
    while j < b.len() {
        for c in [b.get(j - 1), b.get(j)].into_iter().flatten() {
            if !c.is_ascii_hexdigit() {
                return bad(*c);
            }
        }
        j += 2;
    }
    if let Some(&c) = b.get(j - 1).filter(|_| b.len() % 2 == 1)
        && !c.is_ascii_hexdigit()
    {
        return bad(c);
    }
    "encoding/hex: odd length hex string".into()
}

/// %#U of a byte as a rune: U+XXXX, and the rune quoted where it is printable.
fn sharp_u(c: u8) -> String {
    let printable = (0x20..0x7f).contains(&c) || (c >= 0xa1 && c != 0xad);
    match char::from_u32(u32::from(c)).filter(|_| printable) {
        Some(r) => format!("U+{:04X} '{r}'", u32::from(c)),
        None => format!("U+{:04X}", u32::from(c)),
    }
}

/// gitsign.VerifySignature with no policy: an OpenPGP signature as
/// VerifyArmoredDetachedSignature verifies it, or an SSH one: version 1, SHA-256 or
/// SHA-512, namespace "git", by the first key of `keys` (an authorized_keys file), as
/// sshsig.Verify verifies it.
pub fn verify_git_signature(
    signature: &[u8],
    signed: &[u8],
    keys: &[u8],
    now: i64,
    formats: &Formats<'_>,
) -> Result<(), String> {
    if signature.is_empty() {
        return Err("git object is not signed".into());
    }
    if signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
        crate::parse_armored_detached_signature(signature)?;
        return verify_armored_detached_signature(signed, signature, keys, now, formats);
    }
    if !signature.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
        return Err("invalid signature format".into());
    }
    let block = pem::decode(signature)
        .filter(|b| b.kind == "SSH SIGNATURE")
        .ok_or("failed to decode ssh signature PEM block")?;
    let sig =
        ssh::parse_signature(&block.bytes).map_err(|e| format!("failed to parse ssh signature: {e}"))?;
    if sig.version != 1 {
        return Err(format!("unsupported SSH signature version: {}", sig.version));
    }
    if sig.hash_algorithm != b"sha256" && sig.hash_algorithm != b"sha512" {
        return Err(format!(
            "unsupported SSH signature hash algorithm: {}",
            String::from_utf8_lossy(&sig.hash_algorithm)
        ));
    }
    if sig.namespace != b"git" {
        return Err(format!(
            "unexpected SSH signature namespace: {}",
            (formats.quote)(&sig.namespace)
        ));
    }
    let key = ssh::parse_authorized_key(keys, formats.quote)
        .map_err(|e| format!("failed to parse ssh public key: {e}"))?;
    ssh::verify(signed, &sig, &key).map_err(|e| format!("failed to verify ssh signature: {e}"))
}
