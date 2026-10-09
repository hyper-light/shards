//! Transparency log entries as sigstore-go v1.2.2's tlog package handles them (entry.go):
//! ParseTransparencyLogEntry and the Rekor v1 or v2 body it holds, ValidateEntry, the
//! entry's accessors, its signed entry timestamp (VerifySET), its inclusion proof and
//! checkpoint (VerifyInclusion for v1, rekor-tiles' VerifyLogEntryWithHash for v2), and
//! verify/tlog.go's hasRekorV1STH; each failure in the words of the code it ports.

mod gocodec;
mod gojson;
mod jcs;
pub mod note;
mod pem;
mod semver;
mod v1;
mod v2;

use std::collections::BTreeMap;

pub use v2::V2Verifier;

use crate::bundle::TlogEntry;
use crate::keys::{self, Details};
use crate::time::{Time, ZERO_SECS, Zone};
use crate::trusted_root::TransparencyLog;
use crate::x509::{Certificate, PublicKey};

/// ErrNilValue.
pub const ERR_NIL_VALUE: &str = "validation error: nil value in transaction log entry";

/// The body an entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    V1(v1::V1),
    V2(v2::V2),
}

/// A parsed entry (tlog.Entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: String,
    pub version: String,
    pub body: Body,
    /// The signed entry timestamp, where the bundle gives a non-empty one.
    pub signed_entry_timestamp: Option<Vec<u8>>,
    pub tle: TlogEntry,
}

/// The key an entry carries (Entry.PublicKey): a certificate or a public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKey {
    Certificate(Box<Certificate>),
    Key(PublicKey),
}

/// ParseTransparencyLogEntry.
pub fn parse_entry(tle: &TlogEntry) -> Result<Entry, String> {
    let has_key_id = matches!(&tle.log_id, Some(Some(k)) if !k.is_empty());
    if tle.canonicalized_body.is_none() || tle.log_index < 0 || !has_key_id || tle.kind_version.is_none() {
        return Err(ERR_NIL_VALUE.into());
    }
    if let Some(proof) = &tle.inclusion_proof {
        match &proof.checkpoint {
            None => return Err("inclusion proof missing required checkpoint".into()),
            Some(e) if e.is_empty() => return Err("inclusion proof checkpoint empty".into()),
            _ => {}
        }
    }
    let body_bytes = tle.canonicalized_body.clone().unwrap_or_default();
    let body = match v2::unmarshal(&body_bytes) {
        Some(e) => Body::V2(e),
        None => Body::V1(
            v1::unmarshal_entry(&body_bytes)
                .map_err(|e| format!("entry body is not a recognizable Rekor v1 or Rekor v2 type: {e}"))?,
        ),
    };
    let (kind, version) = tle.kind_version.clone().unwrap_or_default();
    let signed_entry_timestamp = match &tle.inclusion_promise {
        Some(Some(set)) if !set.is_empty() => Some(set.clone()),
        _ => None,
    };
    Ok(Entry {
        kind,
        version,
        body,
        signed_entry_timestamp,
        tle: tle.clone(),
    })
}

/// ValidateEntry.
pub fn validate_entry(e: &Entry) -> Result<(), String> {
    match &e.body {
        Body::V1(b) => v1::validate(b),
        Body::V2(b) => v2::validate(b),
    }
}

impl Entry {
    /// IntegratedTime: time.Unix of it, the zero Time where it is 0.
    pub fn integrated_time(&self, zone: Zone) -> Time {
        if self.tle.integrated_time == 0 {
            return Time::utc(ZERO_SECS, 0);
        }
        Time::unix(self.tle.integrated_time, zone)
    }

    /// Signature.
    pub fn signature(&self) -> Vec<u8> {
        match &self.body {
            Body::V1(b) => v1::signature(b),
            Body::V2(b) => b.signature.as_ref().map(|s| s.0.clone()).unwrap_or_default(),
        }
    }

    /// PublicKey: the entry's certificate, else its PKIX key; None where it has neither.
    pub fn public_key(&self) -> Option<EntryKey> {
        let raw = match &self.body {
            Body::V1(b) => pem::decode(&v1::key_pem(b)?)?.0.bytes,
            Body::V2(b) => match b.signature.as_ref()?.1.as_ref()?.0.as_ref()? {
                V2Verifier::PublicKey(r) | V2Verifier::Certificate(r) => r.clone(),
            },
        };
        if let Ok(c) = Certificate::parse(&raw) {
            return Some(EntryKey::Certificate(Box::new(c)));
        }
        pem::parse_pkix(&raw).ok().map(EntryKey::Key)
    }

    /// LogKeyID, as its octets.
    pub fn log_key_id(&self) -> Vec<u8> {
        match &self.tle.log_id {
            Some(Some(k)) => k.clone(),
            _ => Vec::new(),
        }
    }

    pub fn log_index(&self) -> i64 {
        self.tle.log_index
    }

    pub fn has_inclusion_promise(&self) -> bool {
        self.signed_entry_timestamp.is_some()
    }

    pub fn has_inclusion_proof(&self) -> bool {
        self.tle.inclusion_proof.is_some()
    }

    pub fn is_rekor_v2(&self) -> bool {
        matches!(self.body, Body::V2(_))
    }

    /// GetHashedRekordDigest: the digest and its algorithm's name.
    pub fn hashed_rekord_digest(&self) -> Option<(Vec<u8>, String)> {
        match &self.body {
            Body::V1(b) => v1::hashed_rekord_digest(b),
            Body::V2(b) => {
                let (alg, digest) = b.data.as_ref()?;
                Some((digest.clone(), v2::enum_string(&v2::HASH_ALGORITHM, *alg)))
            }
        }
    }

    /// GetDssePayloadHash.
    pub fn dsse_payload_hash(&self) -> Option<Vec<u8>> {
        match &self.body {
            Body::V1(b) => v1::dsse_payload_hash(b),
            Body::V2(_) => None,
        }
    }

    /// The Rekor v2 verifier's key details, where the entry has one.
    pub fn v2_key_details(&self) -> Option<i32> {
        match &self.body {
            Body::V2(b) => Some(b.signature.as_ref()?.1.as_ref()?.1),
            Body::V1(_) => None,
        }
    }
}

/// VerifySET.
pub fn verify_set(e: &Entry, logs: &BTreeMap<String, TransparencyLog>, zone: Zone) -> Result<(), String> {
    if !matches!(e.body, Body::V1(_)) {
        return Err("can only verify SET for Rekor v1 entry".into());
    }
    let log_id = gocodec::hex_encode(&e.log_key_id());
    let body = gocodec::std_encode(e.tle.canonicalized_body.as_deref().unwrap_or_default());
    let log = logs
        .get(&log_id)
        .ok_or("rekor log public key not found for payload")?;
    let Some(start) = log.start else {
        return Err("rekor validity period start time not set".into());
    };
    let it = e.integrated_time(zone);
    if start > it || log.end.is_some_and(|end| end < it) {
        return Err("rekor log public key not valid at payload integrated time".into());
    }
    // json.Marshal of RekorPayload: body, integratedTime, logIndex, logID.
    let payload = format!(
        "{{\"body\":\"{body}\",\"integratedTime\":{},\"logIndex\":{},\"logID\":\"{log_id}\"}}",
        e.tle.integrated_time, e.tle.log_index
    );
    let canonical = jcs::transform(payload.as_bytes()).map_err(|err| format!("canonicalizing: {err}"))?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &canonical);
    let PublicKey::Ecdsa { curve, point } = &log.key else {
        let ty = match log.key {
            PublicKey::Rsa { .. } => "*rsa.PublicKey",
            PublicKey::Ed25519(_) => "ed25519.PublicKey",
            _ => "<nil>",
        };
        return Err(format!("unsupported public key type: {ty}"));
    };
    let set = e.signed_entry_timestamp.as_deref().unwrap_or_default();
    if !crate::x509::ecdsa_asn1(*curve, point, digest.as_ref(), set) {
        return Err("unable to verify SET".into());
    }
    Ok(())
}

/// VerifyInclusion (Rekor v1): the inclusion proof, then the checkpoint's signature
/// and its root.
pub fn verify_inclusion_v1(e: &Entry, verifier: &keys::Verifier) -> Result<(), String> {
    let Some(proof) = &e.tle.inclusion_proof else {
        return Err("inclusion proof not provided".into());
    };
    let leaf = note::hash_leaf(e.tle.canonicalized_body.as_deref().unwrap_or_default());
    note::verify_inclusion(
        proof.log_index as u64,
        proof.tree_size as u64,
        &leaf,
        &proof.hashes,
        &proof.root_hash,
    )?;
    note::verify_checkpoint_signature(
        proof.checkpoint.as_deref().unwrap_or_default().as_bytes(),
        &proof.root_hash,
        verifier,
    )
}

/// hasRekorV1STH: a checkpoint of four lines or more whose origin ends ` - <tree ID>`.
pub fn has_rekor_v1_sth(e: &Entry) -> bool {
    let envelope = e
        .tle
        .inclusion_proof
        .as_ref()
        .and_then(|p| p.checkpoint.as_deref())
        .unwrap_or_default();
    let lines: Vec<&str> = envelope.split('\n').collect();
    if lines.len() < 4 {
        return false;
    }
    let first = lines.first().copied().unwrap_or_default();
    let digits = first.len() - first.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    digits > 0
        && first
            .get(..first.len() - digits)
            .is_some_and(|head| head.ends_with(" - "))
}

/// The Rekor v2 entry checked against its log: the note verifier for `origin`, the
/// checkpoint it signed, and `entry_hash`'s inclusion; in sigstore-go's words
/// (`loading note verifier: …`, `verifying log entry: …`).
pub fn verify_v2(
    e: &Entry,
    origin: &str,
    verifier: &keys::Verifier,
    entry_hash: &[u8],
) -> Result<(), String> {
    let nv =
        note::new_note_verifier(origin, verifier).map_err(|err| format!("loading note verifier: {err}"))?;
    let inner = (|| {
        let envelope = e
            .tle
            .inclusion_proof
            .as_ref()
            .and_then(|p| p.checkpoint.as_deref())
            .unwrap_or_default();
        let cp = note::verify_checkpoint(envelope.as_bytes(), &nv)?;
        if e.tle.log_index < 0 {
            return Err(format!("invalid index: negative integer: {}", e.tle.log_index));
        }
        let hashes = e
            .tle
            .inclusion_proof
            .as_ref()
            .map(|p| p.hashes.clone())
            .unwrap_or_default();
        note::verify_inclusion(e.tle.log_index as u64, cp.size, entry_hash, &hashes, &cp.hash)
            .map_err(|err| format!("verifying inclusion: {err}"))
    })();
    inner.map_err(|err| format!("verifying log entry: {err}"))
}

/// hashedrekord.ToEntryHash.
pub fn v2_entry_hash(
    digest: &[u8],
    signature: &[u8],
    verifier: &V2Verifier,
    key_details: Details,
) -> Result<Vec<u8>, String> {
    v2::to_entry_hash(digest, signature, verifier, key_details)
}

/// The note verifier's hash for a Rekor v1 log (the log's SignatureHashFunc), for callers
/// loading its verifier.
pub fn checkpoint_hash(key: &PublicKey) -> crate::x509::Hash {
    note::signature_hash(key)
}
