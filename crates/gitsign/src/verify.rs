//! OpenPGP signatures verified as go-crypto v1.4.1 verifies them (packet/public_key.go
//! VerifySignature, and the hashes of key, user ID, revocation and direct-key
//! signatures). ECDSA on the NIST curves and secp256k1, and Ed25519, through AWS-LC; RSA,
//! DSA, brainpool's ECDSA and Ed448 by `arith`, as Go's own checks make them.

use aws_lc_rs::digest;

use crate::Error;
use crate::arith;
use crate::key::{self, Curve, Material, PublicKey};
use crate::signature::{Hash, Signature, Values};

fn sig_err(s: &str) -> Error {
    Error::Signature(s.into())
}

fn algorithm(h: Hash) -> &'static digest::Algorithm {
    match h {
        Hash::Sha1 => &digest::SHA1_FOR_LEGACY_USE_ONLY,
        Hash::Sha224 => &digest::SHA224,
        Hash::Sha256 => &digest::SHA256,
        Hash::Sha384 => &digest::SHA384,
        Hash::Sha512 => &digest::SHA512,
        Hash::Sha3_256 => &digest::SHA3_256,
        Hash::Sha3_512 => &digest::SHA3_512,
    }
}

/// The hash a signature names, begun as PrepareVerify begins it: a v6 signature's salt
/// first.
#[derive(Clone)]
pub struct Hasher {
    ctx: digest::Context,
}

impl std::fmt::Debug for Hasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hasher")
    }
}

impl Hasher {
    pub fn new(sig: &Signature) -> Result<Hasher, Error> {
        let hash = sig
            .hash
            .ok_or_else(|| Error::Unsupported("hash function".into()))?;
        let mut ctx = digest::Context::new(algorithm(hash));
        if sig.version == 6 {
            if sig.salt.is_empty() {
                return Err(Error::Structural(
                    "v6 requires a salt for the hash to be signed".into(),
                ));
            }
            ctx.update(&sig.salt);
        }
        Ok(Hasher { ctx })
    }

    pub fn update(&mut self, data: &[u8]) {
        self.ctx.update(data);
    }
}

/// What a signature is checked against: a hash still open, to which its hash suffix is
/// added, or a digest made elsewhere (BuildKit's staticHash, whose writes are dropped).
#[derive(Debug)]
pub enum Signed {
    Open(Hasher),
    Digest(Vec<u8>),
}

/// CheckKeyIdOrFingerprint.
pub fn check_key_id_or_fingerprint(sig: &Signature, pk: &PublicKey) -> bool {
    if let Some(fp) = sig.issuer_fingerprint.as_ref().filter(|f| f.len() >= 20) {
        return *fp == pk.fingerprint;
    }
    sig.issuer_key_id == Some(pk.key_id)
}

fn der_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
}

/// DER of an INTEGER from unsigned big-endian octets.
fn der_int(b: &[u8]) -> Vec<u8> {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    let b = b.get(i..).unwrap_or_default();
    let mut v = Vec::with_capacity(b.len() + 1);
    if b.first().is_none_or(|x| x & 0x80 != 0) {
        v.push(0);
    }
    v.extend_from_slice(b);
    let mut out = vec![0x02];
    der_len(&mut out, v.len());
    out.extend(v);
    out
}

/// An ECDSA signature's (r, s), as DER.
fn der_ecdsa(r: &[u8], s: &[u8]) -> Vec<u8> {
    let body = [der_int(r), der_int(s)].concat();
    let mut out = vec![0x30];
    der_len(&mut out, body.len());
    out.extend(body);
    out
}

/// ecdsa.Verify on a curve AWS-LC knows. ECDSA reads a digest only as an integer, cut to
/// the order's bits: the digest is cut to the order's octets (each of these orders a
/// whole number of octets, or, for P-521, longer than any digest) and padded with leading
/// zeros to the length of the curve's own algorithm, which leaves that integer as it is.
pub(crate) fn ecdsa_aws(curve: Curve, point: &[u8], hashed: &[u8], r: &[u8], s: &[u8]) -> bool {
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_ASN1, ECDSA_P256K1_SHA256_ASN1, ECDSA_P384_SHA384_ASN1, ECDSA_P521_SHA512_ASN1,
        ParsedPublicKey, VerificationAlgorithm,
    };
    let (alg, order, len, dalg): (
        &'static dyn VerificationAlgorithm,
        usize,
        usize,
        &'static digest::Algorithm,
    ) = match curve {
        Curve::P256 => (&ECDSA_P256_SHA256_ASN1, 32, 32, &digest::SHA256),
        Curve::Secp256k1 => (&ECDSA_P256K1_SHA256_ASN1, 32, 32, &digest::SHA256),
        Curve::P384 => (&ECDSA_P384_SHA384_ASN1, 48, 48, &digest::SHA384),
        Curve::P521 => (&ECDSA_P521_SHA512_ASN1, 66, 64, &digest::SHA512),
        _ => return false,
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
    // ecdsa.Verify refuses r or s of zero before it encodes them.
    if r.iter().all(|b| *b == 0) || s.iter().all(|b| *b == 0) {
        return false;
    }
    ParsedPublicKey::new(alg, point).is_ok_and(|k| k.verify_digest_sig(&d, &der_ecdsa(r, s)).is_ok())
}

fn ed25519(public: &[u8], message: &[u8], sig: &[u8]) -> bool {
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
    public.len() == 32
        && UnparsedPublicKey::new(&ED25519, public)
            .verify(message, sig)
            .is_ok()
}

/// VerifySignature: `signed`, with the signature's hash suffix, checked against `sig` by
/// `pk`.
pub fn verify_signature(pk: &PublicKey, signed: Signed, sig: &Signature) -> Result<(), Error> {
    if !pk.can_sign() {
        return Err(Error::InvalidArgument(
            "public key cannot generate signatures".into(),
        ));
    }
    let hashed = match signed {
        Signed::Open(mut h) => {
            h.update(&sig.hash_suffix);
            h.ctx.finish().as_ref().to_vec()
        }
        Signed::Digest(d) => d,
    };
    if sig.version >= 5 && hashed.get(..2) != Some(&sig.hash_tag[..]) {
        return Err(sig_err("hash tag doesn't match"));
    }
    if pk.algo != sig.pubkey_algo {
        return Err(Error::InvalidArgument(
            "public key and signature use different algorithms".into(),
        ));
    }
    let hash = sig
        .hash
        .ok_or_else(|| Error::Unsupported("hash function".into()))?;
    match (&pk.material, &sig.values) {
        (Material::Rsa { n, e }, Some(Values::Rsa(m))) => {
            // rsa.VerifyPKCS1v15 takes a digest of the hash's size only.
            let ok = hashed.len() == algorithm(hash).output_len()
                && arith::rsa_pkcs1_verify(
                    &n.bytes,
                    &e.bytes,
                    hash,
                    &hashed,
                    &pad_to_key_size(&n.bytes, &m.bytes),
                );
            ok.then_some(())
                .ok_or_else(|| sig_err("RSA verification failure"))
        }
        (Material::Dsa { p, q, g, y }, Some(Values::Dsa(r, s))) => {
            // FIPS 186-3 §4.6: the digest cut to the subgroup's octets.
            let q_bits = num_bits(&q.bytes);
            let cut = hashed
                .get(..q_bits.div_ceil(8).min(hashed.len()))
                .unwrap_or_default();
            arith::dsa_verify(&p.bytes, &q.bytes, &g.bytes, &y.bytes, cut, &r.bytes, &s.bytes)
                .then_some(())
                .ok_or_else(|| sig_err("DSA verification failure"))
        }
        (Material::Ecdsa { curve, point, .. }, Some(Values::Ecdsa(r, s))) => {
            let ok = match arith::brainpool(*curve) {
                Some(c) => c
                    .point(&point.bytes)
                    .is_some_and(|(x, y)| c.verify(&x, &y, &hashed, &r.bytes, &s.bytes)),
                None => ecdsa_aws(*curve, &point.bytes, &hashed, &r.bytes, &s.bytes),
            };
            ok.then_some(())
                .ok_or_else(|| sig_err("ECDSA verification failure"))
        }
        (Material::EdDsa { curve, point, .. }, Some(Values::EdDsa(r, s))) => {
            let public = point.bytes.get(1..).unwrap_or_default();
            let ok = match curve {
                // eddsa.Verify: R and S each 32 octets, padded on the left.
                Curve::Ed25519Legacy => {
                    let fits = r.bytes.len() <= 32 && s.bytes.len() <= 32;
                    let mut full = [0u8; 64];
                    if fits {
                        if let Some(d) = full.get_mut(32 - r.bytes.len()..32) {
                            d.copy_from_slice(&r.bytes);
                        }
                        if let Some(d) = full.get_mut(64 - s.bytes.len()..) {
                            d.copy_from_slice(&s.bytes);
                        }
                    }
                    fits && ed25519(public, &hashed, &full)
                }
                // ed448's UnmarshalSignature: R, prefixed, is the whole signature.
                _ => {
                    r.bytes.len() == 115
                        && arith::ed448_verify(public, &hashed, r.bytes.get(1..).unwrap_or_default())
                }
            };
            ok.then_some(())
                .ok_or_else(|| sig_err("EdDSA verification failure"))
        }
        (Material::Native(x), Some(Values::Native(s))) if pk.algo == key::ED25519 => ed25519(x, &hashed, s)
            .then_some(())
            .ok_or_else(|| sig_err("Ed25519 verification failure")),
        (Material::Native(x), Some(Values::Native(s))) if pk.algo == key::ED448 => {
            arith::ed448_verify(x, &hashed, s)
                .then_some(())
                .ok_or_else(|| sig_err("ed448 verification failure"))
        }
        _ => Err(sig_err("Unsupported public key algorithm used in signature")),
    }
}

fn num_bits(b: &[u8]) -> usize {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    match b.get(i) {
        None => 0,
        Some(top) => (b.len() - i - 1) * 8 + (8 - top.leading_zeros() as usize),
    }
}

/// padToKeySize: the signature padded on the left to the modulus's octets.
fn pad_to_key_size(n: &[u8], sig: &[u8]) -> Vec<u8> {
    let k = num_bits(n).div_ceil(8);
    if sig.len() >= k {
        return sig.to_vec();
    }
    let mut out = vec![0u8; k - sig.len()];
    out.extend_from_slice(sig);
    out
}

/// keySignatureHash: the signing key, then the key it signs.
fn key_hash(pk: &PublicKey, signed: &PublicKey, sig: &Signature) -> Result<Hasher, Error> {
    let mut h = Hasher::new(sig)?;
    let mut data = Vec::new();
    pk.serialize_for_hash(&mut data);
    signed.serialize_for_hash(&mut data);
    h.update(&data);
    Ok(h)
}

/// VerifyKeySignature: a subkey's binding or revocation, and a signing subkey's
/// cross-signature.
pub fn key_signature(pk: &PublicKey, signed: &PublicKey, sig: &Signature) -> Result<(), Error> {
    verify_signature(pk, Signed::Open(key_hash(pk, signed, sig)?), sig)?;
    if sig.flags_valid && sig.flags & 0x02 != 0 {
        let Some(embedded) = &sig.embedded else {
            return Err(Error::Structural(
                "signing subkey is missing cross-signature".into(),
            ));
        };
        Hasher::new(embedded)?;
        let h = key_hash(pk, signed, embedded)
            .map_err(|e| Error::Structural(format!("error while hashing for cross-signature: {e}")))?;
        verify_signature(signed, Signed::Open(h), embedded)
            .map_err(|e| Error::Structural(format!("error while verifying cross-signature: {e}")))?;
    }
    Ok(())
}

/// VerifyRevocationSignature, and VerifyDirectKeySignature, which hashes the same.
pub fn revocation_signature(pk: &PublicKey, sig: &Signature) -> Result<(), Error> {
    let mut h = Hasher::new(sig)?;
    let mut data = Vec::new();
    pk.serialize_for_hash(&mut data);
    h.update(&data);
    verify_signature(pk, Signed::Open(h), sig)
}

pub fn direct_key_signature(pk: &PublicKey, sig: &Signature) -> Result<(), Error> {
    revocation_signature(pk, sig)
}

/// VerifyUserIdSignature: the key, then the user ID (0xb4, its length, its octets).
pub fn user_id_signature(pk: &PublicKey, id: &[u8], sig: &Signature) -> Result<(), Error> {
    let mut h = Hasher::new(sig)?;
    let mut data = Vec::new();
    pk.serialize_for_hash(&mut data);
    data.push(0xb4);
    data.extend_from_slice(&u32::try_from(id.len()).unwrap_or(u32::MAX).to_be_bytes());
    data.extend_from_slice(id);
    h.update(&data);
    verify_signature(pk, Signed::Open(h), sig)
}
