//! Signing with a key as cosign v3.1.3 signs an OCI object (cmd/cosign/cli/sign/sign.go
//! signDigestBundle; internal/key/svkeypair.go; pkg/oci/remote/write.go WriteReferrer,
//! WriteAttestationNewBundleFormat): an in-toto statement of the object's digest, of
//! predicate type `https://sigstore.dev/cosign/sign/v1`, in a DSSE envelope signed as
//! sigstore's default signer for the key signs (LoadDefaultSignerVerifier, Ed25519 keys
//! prehashed, keys.go GetDefaultLoadOptions), in a Sigstore bundle v0.3 that names the key
//! by its hint alone; that bundle the one layer of a referrer of the object. Without a
//! transparency log or timestamp authority, as cosign signs with a signing config that
//! names none (measured, PM M135).

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self as lc, KeyPair as _};

use crate::image::{ARTIFACT_IN_TOTO, ARTIFACT_SIGSTORE_BUNDLE, MEDIA_MANIFEST};

/// The predicate type of cosign's signature statement (pkg/types/predicate.go).
pub const COSIGN_SIGN_PREDICATE: &str = "https://sigstore.dev/cosign/sign/v1";
/// The empty descriptor's type and content (image-spec manifest.md "Guidance for an
/// Empty Descriptor").
pub const EMPTY_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
pub const EMPTY_CONFIG: &[u8] = b"{}";

/// A private key as sigstore's default signer for it signs.
enum Key {
    /// ECDSA, ASN.1 signatures over the curve's own hash: P-256 SHA-256, P-384 SHA-384,
    /// P-521 SHA-512 (GetDefaultPublicKeyDetails).
    Ecdsa(lc::EcdsaKeyPair),
    /// Ed25519ph: the message's SHA-512, signed (RFC 8032 §5.1), as cosign signs with
    /// Ed25519 keys; the 64-byte private key, seed then public key.
    Ed25519ph(Box<[u8; 64]>),
    /// RSA PKCS #1 v1.5 with SHA-256, for 2048, 3072 and 4096-bit keys.
    Rsa(lc::RsaKeyPair),
}

/// A key to sign with: what it signs with, and its public key's PKIX encoding.
pub struct Signer {
    key: Key,
    spki: Vec<u8>,
}

/// Its kind and hint, never its private key.
impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.key {
            Key::Ecdsa(_) => "ECDSA",
            Key::Ed25519ph(_) => "Ed25519ph",
            Key::Rsa(_) => "RSA",
        };
        f.debug_struct("Signer")
            .field("key", &kind)
            .field("hint", &self.hint())
            .finish()
    }
}

impl Drop for Signer {
    fn drop(&mut self) {
        if let Key::Ed25519ph(k) = &mut self.key {
            k.fill(0);
        }
    }
}

impl Signer {
    /// The signer of the PKCS #8 private key `der`: an ECDSA key of P-256, P-384 or
    /// P-521, an Ed25519 key, or an RSA key of 2048, 3072 or 4096 bits.
    pub fn from_pkcs8(der: &[u8]) -> Result<Signer, String> {
        for alg in [
            &lc::ECDSA_P256_SHA256_ASN1_SIGNING,
            &lc::ECDSA_P384_SHA384_ASN1_SIGNING,
            &lc::ECDSA_P521_SHA512_ASN1_SIGNING,
        ] {
            if let Ok(k) = lc::EcdsaKeyPair::from_pkcs8(alg, der) {
                let spki = k
                    .public_key()
                    .as_der()
                    .map_err(|_| "encoding the public key")?
                    .as_ref()
                    .to_vec();
                return Ok(Signer {
                    key: Key::Ecdsa(k),
                    spki,
                });
            }
        }
        if let Ok(k) = lc::Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
            use aws_lc_rs::encoding::AsBigEndian as _;
            let spki = k
                .public_key()
                .as_der()
                .map_err(|_| "encoding the public key")?
                .as_ref()
                .to_vec();
            let seed = k.seed().map_err(|_| "reading the key's seed")?;
            let seed = seed.as_be_bytes().map_err(|_| "reading the key's seed")?;
            let (seed, public) = (seed.as_ref(), k.public_key().as_ref());
            if seed.len() != 32 || public.len() != 32 {
                return Err("an Ed25519 key of other than 32 bytes".into());
            }
            let mut private = Box::new([0u8; 64]);
            for (o, b) in private.iter_mut().zip(seed.iter().chain(public)) {
                *o = *b;
            }
            return Ok(Signer {
                key: Key::Ed25519ph(private),
                spki,
            });
        }
        if let Ok(k) = lc::RsaKeyPair::from_pkcs8(der) {
            let bits = k.public_modulus_len() * 8;
            if ![2048, 3072, 4096].contains(&bits) {
                return Err(format!("an RSA key of {bits} bits: unsupported public key type"));
            }
            let spki = k
                .public_key()
                .as_der()
                .map_err(|_| "encoding the public key")?
                .as_ref()
                .to_vec();
            return Ok(Signer {
                key: Key::Rsa(k),
                spki,
            });
        }
        Err("not an ECDSA (P-256, P-384, P-521), Ed25519 or RSA key in PKCS #8".into())
    }

    /// The public key, as x509.MarshalPKIXPublicKey writes it.
    pub fn public_key_der(&self) -> &[u8] {
        &self.spki
    }

    /// The key's hint, as cosign names it in a bundle: its PKIX encoding's SHA-256, in
    /// standard base64 (NewSignerVerifierKeypair).
    pub fn hint(&self) -> String {
        let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &self.spki);
        crate::tlog::gocodec::std_encode(d.as_ref())
    }

    /// `message` signed.
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let rng = SystemRandom::new();
        match &self.key {
            Key::Ecdsa(k) => k
                .sign(&rng, message)
                .map(|s| s.as_ref().to_vec())
                .map_err(|_| "signing failed".into()),
            Key::Ed25519ph(private) => {
                let mut sig = [0u8; 64];
                // SAFETY: ED25519ph_sign reads the message and the 64-byte private key and
                // writes the 64-byte signature; no context.
                let ok = unsafe {
                    aws_lc_sys::ED25519ph_sign(
                        sig.as_mut_ptr(),
                        message.as_ptr(),
                        message.len(),
                        private.as_ptr(),
                        std::ptr::null(),
                        0,
                    )
                };
                if ok != 1 {
                    return Err("signing failed".into());
                }
                Ok(sig.to_vec())
            }
            Key::Rsa(k) => {
                let mut sig = vec![0u8; k.public_modulus_len()];
                k.sign(&lc::RSA_PKCS1_SHA256, &rng, message, &mut sig)
                    .map_err(|_| "signing failed")?;
                Ok(sig)
            }
        }
    }
}

/// JSON's string of `s` as protojson and encoding/json write it for these documents,
/// whose strings are ASCII digests, types and base64.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The statement cosign signs of the object `digest` (`alg:hex`), as protojson writes
/// in-toto's Statement (sign.go:164-180): the digest its one subject, with no
/// annotations, an empty predicate.
pub fn statement(digest: &str) -> Result<Vec<u8>, String> {
    let (alg, hex) = digest
        .split_once(':')
        .ok_or_else(|| format!("unable to parse digest {digest}"))?;
    Ok(format!(
        r#"{{"_type":"https://in-toto.io/Statement/v1","subject":[{{"digest":{{{}:{}}},"annotations":{{}}}}],"predicateType":{},"predicate":{{}}}}"#,
        quote(alg),
        quote(hex),
        quote(COSIGN_SIGN_PREDICATE)
    )
    .into_bytes())
}

/// DSSE's pre-authentication encoding of a payload of `payload_type` (DSSE v1 protocol):
/// `DSSEv1 <len(type)> <type> <len(body)> <body>`.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {payload_type} {} ", payload_type.len(), payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// The bundle cosign makes of `payload`, an in-toto statement, signed by `signer` with no
/// service: the key named by its hint, and the DSSE envelope, as protojson writes the
/// Bundle (measured: cosign v3.1.3 with an empty signing config).
pub fn bundle(signer: &Signer, payload: &[u8]) -> Result<Vec<u8>, String> {
    let sig = signer.sign(&pae(ARTIFACT_IN_TOTO, payload))?;
    let b64 = crate::tlog::gocodec::std_encode;
    Ok(format!(
        r#"{{"mediaType":{},"verificationMaterial":{{"publicKey":{{"hint":{}}}}},"dsseEnvelope":{{"payload":{},"payloadType":{},"signatures":[{{"sig":{}}}]}}}}"#,
        quote(ARTIFACT_SIGSTORE_BUNDLE),
        quote(&signer.hint()),
        quote(&b64(payload)),
        quote(ARTIFACT_IN_TOTO),
        quote(&b64(&sig))
    )
    .into_bytes())
}

/// What a bundle says of itself before it is verified; verifying it is what makes any of
/// it true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unverified {
    /// Whether a key signed it: its material a key's hint, not a certificate.
    pub keyed: bool,
    /// Its envelope's predicate type, if it holds an in-toto statement.
    pub predicate: Option<String>,
    /// Whether it holds transparency log entries, which only a trusted root verifies.
    pub logged: bool,
}

/// [`Unverified`] of `bundle`.
pub fn unverified(bundle: &[u8]) -> Result<Unverified, crate::Error> {
    let b = crate::bundle::parse(bundle)?;
    let predicate = match b.signature_content() {
        Ok(crate::bundle::SignatureContent::Envelope(env)) => {
            crate::signature::statement(&env).ok().map(|s| s.predicate_type)
        }
        _ => None,
    };
    Ok(Unverified {
        keyed: matches!(b.material, crate::bundle::Material::PublicKey { .. }),
        predicate,
        logged: !b.entries.is_empty(),
    })
}

/// The key a PEM block of type `PUBLIC KEY` holds, as cosign.pub holds it, loaded as
/// `cosign verify --key` loads one (cryptoutils.UnmarshalPEMToPublicKey, then the key's
/// default verifier, Ed25519 keys prehashed: GetDefaultLoadOptions).
pub fn public_key(pem: &[u8]) -> Result<crate::verify::KeyMaterial, String> {
    let (block, _) = crate::tlog::pem::decode(pem).ok_or("PEM decoding failed")?;
    if block.kind != b"PUBLIC KEY" {
        return Err(format!(
            "unknown Public key PEM file type: {}",
            String::from_utf8_lossy(&block.kind)
        ));
    }
    key_of_spki(&block.bytes)
}

/// A key's PKIX encoding as cosign.pub holds it: a `PUBLIC KEY` PEM block.
pub fn public_key_pem(spki: &[u8]) -> Vec<u8> {
    crate::tlog::pem::encode("PUBLIC KEY", spki)
}

/// [`public_key`] of a key's PKIX encoding.
pub fn key_of_spki(spki: &[u8]) -> Result<crate::verify::KeyMaterial, String> {
    let key = crate::tlog::pem::parse_pkix(spki)?;
    Ok(crate::verify::KeyMaterial {
        verifier: crate::keys::load_default(&key, true)?,
        valid_from: 0,
    })
}

/// What `cosign verify --key` checks of a signature bundle of the object `digest`
/// (`alg:hex`) by the key `key` (pkg/cosign/verify.go: WithKey, the artifact's digest):
/// the bundle signed with a key, its envelope's signature by `key`, its statement of
/// cosign's signature predicate with `digest` among its subjects; the transparency log
/// entries it holds, if any, verified against `root` (WithTransparencyLog), none required,
/// as cosign verifies a signature it uploaded none for (`--insecure-ignore-tlog`); no
/// timestamp (WithNoObserverTimestamps).
pub fn verify_key_signed(
    bundle: &[u8],
    digest: &str,
    key: &crate::verify::KeyMaterial,
    root: &crate::trusted_root::TrustedRoot,
    zone: crate::time::Zone,
) -> Result<crate::verify::Outcome, crate::Error> {
    use crate::verify::{Config, Entity, Identity, Material, Policy};
    let b = crate::bundle::parse(bundle)?;
    let (alg, hex) = digest
        .split_once(':')
        .ok_or_else(|| crate::Error(format!("unable to parse digest {digest}")))?;
    let raw = crate::signature::hex_decode(hex).ok_or_else(|| {
        crate::Error(format!(
            "decoding digest {digest}: {}",
            crate::signature::hex_error(hex)
        ))
    })?;
    let config = Config {
        tlog: usize::from(!b.entries.is_empty()),
        no_observer: true,
        ..Config::default()
    };
    let material = Material {
        root,
        key: Some(key),
        fulcio: false,
    };
    let policy = Policy {
        digest: Some((alg.to_string(), raw)),
        identity: Identity::Unsafe,
    };
    let outcome = crate::verify::verify_entity(&Entity::from_bundle(&b), &material, &config, &policy, zone)?;
    match &outcome.statement {
        Some(s) if s.predicate_type == COSIGN_SIGN_PREDICATE => Ok(outcome),
        Some(s) => Err(crate::Error(format!(
            "a statement of predicate type {} is no signature",
            s.predicate_type
        ))),
        None => Err(crate::Error("a signature with no statement".into())),
    }
}

/// An OCI descriptor's fields cosign writes: its type, size and digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    pub media_type: String,
    pub size: i64,
    pub digest: String,
}

/// The referrer cosign writes of a bundle (WriteAttestationNewBundleFormat, WriteReferrer):
/// json.Marshal of go-containerregistry's manifest and its artifact type, the empty
/// config of the bundle's type, the bundle its one layer, cosign's annotations, `created`
/// as RFC 3339 in UTC to the second, and `subject`.
pub fn referrer(bundle: &Described, subject: &Described, predicate_type: &str, created: &str) -> Vec<u8> {
    let empty = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, EMPTY_CONFIG);
    let empty: String = empty.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let descriptor = |d: &Described| {
        format!(
            r#"{{"mediaType":{},"size":{},"digest":{}}}"#,
            quote(&d.media_type),
            d.size,
            quote(&d.digest)
        )
    };
    format!(
        r#"{{"schemaVersion":2,"mediaType":{},"config":{{"mediaType":{},"size":{},"digest":{},"artifactType":{}}},"layers":[{}],"annotations":{{"dev.sigstore.bundle.content":"dsse-envelope","dev.sigstore.bundle.predicateType":{},"org.opencontainers.image.created":{}}},"subject":{},"artifactType":{}}}"#,
        quote(MEDIA_MANIFEST),
        quote(EMPTY_MEDIA_TYPE),
        EMPTY_CONFIG.len(),
        quote(&format!("sha256:{empty}")),
        quote(ARTIFACT_SIGSTORE_BUNDLE),
        descriptor(bundle),
        quote(predicate_type),
        quote(created),
        descriptor(subject),
        quote(ARTIFACT_SIGSTORE_BUNDLE)
    )
    .into_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use crate::trusted_root::TrustedRoot;
    use crate::verify::KeyMaterial;

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    fn sha256(b: &[u8]) -> String {
        let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b);
        let hex: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }

    /// The key a PKIX PEM block holds, as cosign verify --key loads it (Ed25519 keys
    /// prehashed, GetDefaultLoadOptions).
    fn key_of_pem(pem: &[u8]) -> KeyMaterial {
        public_key(pem).unwrap()
    }

    fn key_of_spki(spki: &[u8]) -> KeyMaterial {
        super::key_of_spki(spki).unwrap()
    }

    fn measured() -> serde_json::Value {
        json(include_str!("../testdata/cosign/measured.json"))
    }

    /// What cosign v3.1.3 wrote (testdata/cosign/measured.json, PM M135): its statement
    /// and its referrer manifest are this module's, byte for byte, given the bundle and
    /// the time it wrote; its bundle verifies with its key.
    #[test]
    fn cosigns_signature_is_made_and_verified_alike() {
        let m = measured();
        let subject = m["subject"]["digest"].as_str().unwrap();
        let bundle = m["bundle"].as_str().unwrap();
        let b = json(bundle);
        let payload = crate::tlog::gocodec::STD_ENCODING
            .decode(b["dsseEnvelope"]["payload"].as_str().unwrap().as_bytes())
            .unwrap();
        assert_eq!(payload, statement(subject).unwrap());
        let manifest = m["manifest"].as_str().unwrap();
        let written = json(manifest);
        let made = referrer(
            &Described {
                media_type: ARTIFACT_SIGSTORE_BUNDLE.into(),
                size: i64::try_from(bundle.len()).unwrap(),
                digest: sha256(bundle.as_bytes()),
            },
            &Described {
                media_type: m["subject"]["mediaType"].as_str().unwrap().into(),
                size: m["subject"]["size"].as_i64().unwrap(),
                digest: subject.into(),
            },
            COSIGN_SIGN_PREDICATE,
            written["annotations"]["org.opencontainers.image.created"]
                .as_str()
                .unwrap(),
        );
        assert_eq!(String::from_utf8(made.clone()).unwrap(), manifest);
        assert_eq!(sha256(&made), m["manifest_digest"].as_str().unwrap());
        let key = key_of_pem(m["public_key"].as_str().unwrap().as_bytes());
        let root = TrustedRoot::default();
        let outcome = verify_key_signed(bundle.as_bytes(), subject, &key, &root, crate::time::utc).unwrap();
        assert_eq!(
            outcome.public_key_id.as_deref(),
            b["verificationMaterial"]["publicKey"]["hint"].as_str()
        );
        // Not of another object.
        let other = format!("sha256:{}", "0".repeat(64));
        assert!(verify_key_signed(bundle.as_bytes(), &other, &key, &root, crate::time::utc).is_err());
    }

    /// A bundle made with each key cosign signs with verifies, with that key, as cosign
    /// verify --key verifies it, and not with another, nor for another object, nor once
    /// its envelope is changed.
    #[test]
    fn bundles_of_each_key_type_verify() {
        let oracle = json(include_str!("../testdata/cosign/oracle.json"));
        let digest = format!("sha256:{}", "ab".repeat(32));
        let root = TrustedRoot::default();
        let mut keys = Vec::new();
        for k in oracle["keys"].as_array().unwrap() {
            let name = k["name"].as_str().unwrap();
            if !name.ends_with(" standard") {
                continue;
            }
            let der: Vec<u8> =
                crate::tlog::gocodec::hex_decode(k["pkcs8"].as_str().unwrap().as_bytes()).unwrap();
            keys.push((name.to_string(), Signer::from_pkcs8(&der).unwrap()));
        }
        assert_eq!(keys.len(), 5);
        for (i, (name, signer)) in keys.iter().enumerate() {
            let b = bundle(signer, &statement(&digest).unwrap()).unwrap();
            let key = key_of_spki(signer.public_key_der());
            let outcome = verify_key_signed(&b, &digest, &key, &root, crate::time::utc)
                .unwrap_or_else(|e| panic!("{name}: {}", e.0));
            assert_eq!(
                outcome.public_key_id.as_deref(),
                Some(signer.hint().as_str()),
                "{name}"
            );
            let other = &keys[(i + 1) % keys.len()].1;
            let wrong = key_of_spki(other.public_key_der());
            assert!(
                verify_key_signed(&b, &digest, &wrong, &root, crate::time::utc).is_err(),
                "{name}"
            );
            let elsewhere = format!("sha256:{}", "cd".repeat(32));
            assert!(
                verify_key_signed(&b, &elsewhere, &key, &root, crate::time::utc).is_err(),
                "{name}"
            );
            let mut v = json(std::str::from_utf8(&b).unwrap());
            let tampered = statement(&elsewhere).unwrap();
            v["dsseEnvelope"]["payload"] = crate::tlog::gocodec::std_encode(&tampered).into();
            let tampered = serde_json::to_vec(&v).unwrap();
            assert!(
                verify_key_signed(&tampered, &elsewhere, &key, &root, crate::time::utc).is_err(),
                "{name}"
            );
        }
    }

    /// A bundle holding transparency log entries has them verified, against the root it
    /// is given: a real entry (BuildKit v0.28.1's, testdata/real) put in cosign's
    /// signature, which no root here holds the log of, refuses it.
    #[test]
    fn a_signatures_log_entries_are_verified() {
        let m = measured();
        let subject = m["subject"]["digest"].as_str().unwrap();
        let key = key_of_pem(m["public_key"].as_str().unwrap().as_bytes());
        let real = json(include_str!(
            "../testdata/real/buildkit-v0.28.1-arm64.bundle.json"
        ));
        let entries = real["verificationMaterial"]["tlogEntries"].clone();
        assert!(entries.as_array().is_some_and(|e| !e.is_empty()));
        let mut b = json(m["bundle"].as_str().unwrap());
        b["verificationMaterial"]["tlogEntries"] = entries;
        let logged = serde_json::to_vec(&b).unwrap();
        let root = TrustedRoot::default();
        let e = verify_key_signed(&logged, subject, &key, &root, crate::time::utc).unwrap_err();
        assert!(e.0.contains("log inclusion"), "{}", e.0);
    }

    /// A statement of another predicate the same key signed of the same object, an
    /// attestation, is no signature.
    #[test]
    fn only_cosigns_signature_predicate_is_a_signature() {
        let oracle = json(include_str!("../testdata/cosign/oracle.json"));
        let k = oracle["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == "ecdsa p256 standard")
            .unwrap();
        let der = crate::tlog::gocodec::hex_decode(k["pkcs8"].as_str().unwrap().as_bytes()).unwrap();
        let signer = Signer::from_pkcs8(&der).unwrap();
        let digest = format!("sha256:{}", "ab".repeat(32));
        let other = String::from_utf8(statement(&digest).unwrap())
            .unwrap()
            .replace(COSIGN_SIGN_PREDICATE, "https://spdx.dev/Document");
        let b = bundle(&signer, other.as_bytes()).unwrap();
        let key = key_of_spki(signer.public_key_der());
        let e = verify_key_signed(&b, &digest, &key, &TrustedRoot::default(), crate::time::utc).unwrap_err();
        assert_eq!(
            e.0,
            "a statement of predicate type https://spdx.dev/Document is no signature"
        );
    }

    #[test]
    fn pae_is_dsses() {
        assert_eq!(
            pae("application/vnd.in-toto+json", b"{}"),
            b"DSSEv1 28 application/vnd.in-toto+json 2 {}".to_vec()
        );
    }
}
