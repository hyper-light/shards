//! A bundle's signature checked as sigstore-go checks it (verify/signature.go): the
//! verifier for its certificate or key (compatSignatureVerifier, getSignatureVerifier),
//! DSSE envelopes as go-securesystemslib's EnvelopeVerifier verifies them over the PAE
//! (key IDs as SSH fingerprints, as SHA256KeyID makes them), the in-toto statement as
//! protojson reads it, and the artifact's digest against a message signature or the
//! statement's subjects.

use aws_lc_rs::digest;
use base64::Engine as _;

use crate::bundle::{Envelope, SignatureContent};
use crate::keys::{self, Details, Load, VerifyWith};
use crate::proto::{self, Val};
use crate::schemas;
use crate::verify::Statement;
use crate::x509::{Curve, Hash, PublicKey};

/// A signature verifier: one, or compatVerifier's list, tried in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigVerifier {
    Single(keys::Verifier),
    Compat(Vec<keys::Verifier>),
}

impl SigVerifier {
    /// VerifySignature.
    pub fn verify(&self, sig: &[u8], message: &[u8], with: &VerifyWith<'_>) -> Result<(), String> {
        match self {
            SigVerifier::Single(v) => v.verify(sig, message, with),
            SigVerifier::Compat(vs) => {
                if vs.iter().any(|v| v.verify(sig, message, with).is_ok()) {
                    Ok(())
                } else {
                    Err("no compatible verifier found".into())
                }
            }
        }
    }

    /// PublicKey: the first verifier's.
    pub fn public_key(&self) -> Option<PublicKey> {
        match self {
            SigVerifier::Single(v) => Some(v.public_key()),
            SigVerifier::Compat(vs) => vs.first().map(keys::Verifier::public_key),
        }
    }

    fn is_plain_ed25519(&self) -> bool {
        matches!(self, SigVerifier::Single(v) if v.is_ed25519())
    }
}

/// compatSignatureVerifier.
pub fn compat_verifier(key: &PublicKey, enable_compat: bool, is_dsse: bool) -> Result<SigVerifier, String> {
    let ed25519ph = !is_dsse;
    let verifier = keys::load_default(key, ed25519ph)?;
    if !enable_compat {
        return Ok(SigVerifier::Single(verifier));
    }
    let mut verifiers = vec![verifier.clone()];
    let second = match key {
        PublicKey::Ecdsa { curve, .. } => {
            let details = match curve {
                Curve::P384 => Details::EcdsaP384Sha256,
                Curve::P521 => Details::EcdsaP521Sha256,
                _ => return Ok(SigVerifier::Single(verifier)),
            };
            keys::load_from_details(key, details, ed25519ph, None)?
        }
        // Not ECDSA: the default verifier, a second time.
        _ => verifier,
    };
    verifiers.push(second);
    Ok(SigVerifier::Compat(verifiers))
}

/// ssh.NewPublicKey's wire form of a key, or None for a key SSH does not take.
fn ssh_wire(key: &PublicKey) -> Option<Vec<u8>> {
    fn string(out: &mut Vec<u8>, b: &[u8]) {
        out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(b);
    }
    fn mpint(out: &mut Vec<u8>, b: &[u8]) {
        let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
        let mut v = b.get(i..).unwrap_or_default().to_vec();
        if v.first().is_some_and(|x| x & 0x80 != 0) {
            v.insert(0, 0);
        }
        string(out, &v);
    }
    let mut out = Vec::new();
    match key {
        PublicKey::Rsa { n, e } => {
            string(&mut out, b"ssh-rsa");
            mpint(&mut out, e);
            mpint(&mut out, n);
        }
        PublicKey::Ecdsa { curve, point } => {
            let name: &[u8] = match curve {
                Curve::P256 => b"nistp256",
                Curve::P384 => b"nistp384",
                Curve::P521 => b"nistp521",
                Curve::P224 => return None,
            };
            string(&mut out, &[b"ecdsa-sha2-".as_slice(), name].concat());
            string(&mut out, name);
            string(&mut out, point);
        }
        PublicKey::Ed25519(k) => {
            string(&mut out, b"ssh-ed25519");
            string(&mut out, k);
        }
        _ => return None,
    }
    Some(out)
}

/// SHA256KeyID: the key's SSH SHA-256 fingerprint.
pub fn sha256_key_id(key: &PublicKey) -> Option<String> {
    let wire = ssh_wire(key)?;
    let sum = digest::digest(&digest::SHA256, &wire);
    Some(format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(sum.as_ref())
    ))
}

/// go-securesystemslib's b64Decode: standard base64, else URL-safe.
fn b64(s: &str) -> Result<Vec<u8>, String> {
    crate::gobase64::decode(s.as_bytes(), false, true)
        .or_else(|_| crate::gobase64::decode(s.as_bytes(), true, true))
        .map_err(|_| "unable to base64 decode payload (is payload in the right format?)".to_string())
}

/// PAE.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut b = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        payload.len()
    )
    .into_bytes();
    b.extend_from_slice(payload);
    b
}

/// verifyEnvelope: one signature, checked as an EnvelopeVerifier of the one verifier
/// checks it.
pub fn verify_envelope(verifier: &SigVerifier, env: &Envelope) -> Result<(), String> {
    if env.signatures.len() != 1 {
        return Err("exactly one signature is required".into());
    }
    let body = b64(&env.payload).map_err(|e| format!("could not verify envelope: {e}"))?;
    let message = pae(&env.payload_type, &body);
    let key_id = verifier.public_key().as_ref().and_then(sha256_key_id);
    let mut accepted = 0;
    for (sig_key_id, sig) in &env.signatures {
        let sig = b64(sig).map_err(|e| format!("could not verify envelope: {e}"))?;
        if accepted > 0 {
            break;
        }
        if let Some(k) = &key_id
            && !sig_key_id.is_empty()
            && sig_key_id != k
        {
            continue;
        }
        if verifier.verify(&sig, &message, &VerifyWith::default()).is_ok() {
            accepted += 1;
        }
    }
    if accepted < 1 {
        return Err(format!(
            "could not verify envelope: accepted signatures do not match threshold, Found: {accepted}, Expected 1"
        ));
    }
    Ok(())
}

/// The envelope's in-toto statement (Envelope.Statement).
pub fn statement(env: &Envelope) -> Result<Statement, String> {
    if env.payload_type != "application/vnd.in-toto+json" {
        return Err("validation error: unsupported media type".into());
    }
    let raw = b64(&env.payload)
        .map_err(|_| "validation error: invalid attestation: decoding base64".to_string())?;
    let m = proto::unmarshal(&raw, &schemas::STATEMENT)
        .map_err(|_| "validation error: invalid attestation: decoding json".to_string())?;
    let subjects = m
        .list(2)
        .iter()
        .filter_map(|v| match v {
            Val::Msg(s) => Some((
                s.string(1),
                match s.get(3) {
                    Some(Val::Map(d)) => d.clone(),
                    _ => Vec::new(),
                },
            )),
            _ => None,
        })
        .collect();
    Ok(Statement {
        predicate_type: m.string(3),
        subjects,
    })
}

/// algStringToHashFunc.
fn alg_hash(alg: &str) -> Result<Hash, String> {
    match alg {
        "sha256" | "SHA2_256" => Ok(Hash::Sha256),
        "sha384" | "SHA2_384" => Ok(Hash::Sha384),
        "sha512" | "SHA2_512" => Ok(Hash::Sha512),
        "" => Err("empty digest algorithm".into()),
        _ => Err("unsupported digest algorithm".into()),
    }
}

/// limitSubjects.
fn limit_subjects(st: &Statement) -> Result<(), String> {
    if st.subjects.len() > 1024 {
        return Err(format!("too many subjects: {} > 1024", st.subjects.len()));
    }
    for (_, digests) in &st.subjects {
        if digests.len() > 32 {
            return Err(format!("too many digests: {} > 32", digests.len()));
        }
    }
    Ok(())
}

/// Go's hex.DecodeString's verdict.
pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect()
}

/// verifyEnvelopeWithArtifactDigests (one digest, as the policy helpers give).
pub fn verify_envelope_digest(
    verifier: &SigVerifier,
    env: &Envelope,
    alg: &str,
    digest: &[u8],
) -> Result<(), String> {
    verify_envelope(verifier, env)?;
    let st = statement(env)
        .map_err(|e| format!("could not verify artifact: unable to extract statement from envelope: {e}"))?;
    limit_subjects(&st)?;
    let mut found: Vec<Vec<u8>> = Vec::new();
    for (_, digests) in &st.subjects {
        // Go ranges over the map in a random order; the first undecodable digest of an
        // algorithm fails either way.
        for (a, h) in digests {
            let d = hex_decode(h).ok_or_else(|| {
                format!(
                    "could not verify artifact: unable to decode subject digest: {}",
                    hex_error(h)
                )
            })?;
            if a == alg {
                found.push(d);
            }
        }
    }
    if !st.subjects.iter().any(|(_, ds)| ds.iter().any(|(a, _)| a == alg)) {
        return Err("provided artifact digests does not match digests in statement".into());
    }
    if !found.iter().any(|d| d == digest) {
        return Err("provided artifact digest does not match any digest in statement".into());
    }
    Ok(())
}

/// hex.DecodeString's error: the first invalid octet of the pairs, then of an odd tail,
/// else the odd length.
pub(crate) fn hex_error(s: &str) -> String {
    let b = s.as_bytes();
    let invalid = |c: u8| {
        format!(
            "encoding/hex: invalid byte: U+{:04X} {}",
            u32::from(c),
            quote_rune(c)
        )
    };
    let (pairs, tail) = b.as_chunks::<2>();
    for pair in pairs {
        for &c in pair {
            if !c.is_ascii_hexdigit() {
                return invalid(c);
            }
        }
    }
    if let Some(&last) = tail.first() {
        if !last.is_ascii_hexdigit() {
            return invalid(last);
        }
        return "encoding/hex: odd length hex string".into();
    }
    String::new()
}

/// strconv.QuoteRune of a rune below U+0100.
fn quote_rune(c: u8) -> String {
    match c {
        0x07 => "'\\a'".into(),
        0x08 => "'\\b'".into(),
        0x0c => "'\\f'".into(),
        b'\n' => "'\\n'".into(),
        b'\r' => "'\\r'".into(),
        b'\t' => "'\\t'".into(),
        0x0b => "'\\v'".into(),
        b'\'' => "'\\''".into(),
        b'\\' => "'\\\\'".into(),
        0x00..=0x1f | 0x7f => format!("'\\x{c:02x}'"),
        0x80..=0xa0 | 0xad => format!("'\\u{:04x}'", u32::from(c)),
        _ => format!("'{}'", char::from(c)),
    }
}

/// verifyMessageSignatureWithArtifactDigest.
pub fn verify_message_digest(
    verifier: &SigVerifier,
    digest: &[u8],
    signature: &[u8],
    artifact_digest: &[u8],
) -> Result<(), String> {
    if artifact_digest != digest {
        return Err("artifact does not match digest".into());
    }
    if verifier.is_plain_ed25519() {
        return Err(
            "message signatures with ed25519 signatures can only be verified with artifacts, and not just their digest"
                .into(),
        );
    }
    verifier
        .verify(
            signature,
            &[],
            &VerifyWith {
                digest: Some(artifact_digest),
                hash: None,
            },
        )
        .map_err(|e| format!("could not verify message: {e}"))
}

/// verifySignatureWithVerifierAndArtifactDigests (one digest).
pub fn verify_with_digest(
    verifier: &SigVerifier,
    content: &SignatureContent,
    alg: &str,
    digest: &[u8],
) -> Result<(), String> {
    match content {
        SignatureContent::Envelope(env) => verify_envelope_digest(verifier, env, alg, digest),
        SignatureContent::Message {
            digest: d, signature, ..
        } => verify_message_digest(verifier, d, signature, digest),
    }
}

/// verifySignatureWithVerifier: no artifact.
pub fn verify_without_artifact(verifier: &SigVerifier, content: &SignatureContent) -> Result<(), String> {
    match content {
        SignatureContent::Envelope(env) => verify_envelope(verifier, env),
        SignatureContent::Message { .. } => {
            Err("artifact must be provided to verify message signature".into())
        }
    }
}

/// The hash a message signature's digest algorithm names, for the tlog's comparison.
pub fn message_hash(alg: &str) -> Result<Hash, String> {
    alg_hash(alg)
}

/// A key's verifier with the defaults LoadVerifierWithOpts takes (SHA-256).
pub fn default_verifier(key: &PublicKey) -> Result<keys::Verifier, String> {
    keys::load(key, Load::default())
}
