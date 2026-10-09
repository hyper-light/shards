//! sigstore-go's verifier (verify/signed_entity.go and the checks it calls), as BuildKit's
//! policy helpers configure it.

use crate::Error;
use crate::bundle::{Bundle, Material as BundleMaterial, SignatureContent};
use crate::keys;
use crate::signature;
use crate::summary::Summary;
use crate::time::{Time, Zone};
use crate::tlog::{self, EntryKey};
use crate::trusted_root::TrustedRoot;
use crate::x509::Certificate;

/// VerifierConfig: each threshold 0 where not required.
#[derive(Debug, Clone, Copy, Default)]
pub struct Config {
    pub tlog: usize,
    pub observer: usize,
    pub sct: usize,
    pub signed: usize,
    pub integrated: usize,
    pub no_observer: bool,
}

/// Whom the signature must be from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// Any certificate identity (a SAN and an issuer matching `.*`).
    Any,
    /// WithoutIdentitiesUnsafe.
    Unsafe,
}

/// PolicyConfig: the artifact digest, and the identity.
#[derive(Debug, Clone)]
pub struct Policy {
    pub digest: Option<(String, Vec<u8>)>,
    pub identity: Identity,
}

/// A public key trusted for any hint from a time on (the DHI key's material).
#[derive(Debug, Clone)]
pub struct KeyMaterial {
    pub verifier: keys::Verifier,
    pub valid_from: i64,
}

/// TrustedMaterial: the root, and the public key verifier, if any. `fulcio` false
/// leaves out Fulcio's CAs and the CT logs, as the DHI material does.
#[derive(Debug, Clone, Copy)]
pub struct Material<'a> {
    pub root: &'a TrustedRoot,
    pub key: Option<&'a KeyMaterial>,
    pub fulcio: bool,
}

/// A verified timestamp (TimestampVerificationResult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTimestamp {
    pub kind: &'static str,
    pub uri: String,
    pub time: Time,
}

/// An in-toto statement's parts the policies read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Statement {
    pub predicate_type: String,
    /// Each subject's name and its digests, by algorithm.
    pub subjects: Vec<(String, Vec<(String, String)>)>,
}

/// VerificationResult.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub certificate: Option<Summary>,
    pub public_key_id: Option<String>,
    pub timestamps: Vec<VerifiedTimestamp>,
    pub statement: Option<Statement>,
}

/// VerifierConfig.Validate.
fn validate(config: &Config) -> Result<(), Error> {
    let required = config.observer > 0 || config.signed > 0 || config.integrated > 0;
    if config.no_observer && required {
        return Err(Error(
            "specify WithNoObserverTimestamps() without any other verifier options".into(),
        ));
    }
    if !required && !config.no_observer {
        return Err(Error(
            "when initializing a new Verifier, you must specify at least one of WithObserverTimestamps(), WithSignedTimestamps(), WithIntegratedTimestamps() or WithCurrentTime(), or exclusively specify WithNoObserverTimestamps()"
                .into(),
        ));
    }
    Ok(())
}

/// VerificationContent: the leaf certificate, or the key's hint.
#[derive(Debug)]
enum Content {
    Certificate(Box<Certificate>),
    Key(String),
}

const MISSING_MATERIAL: &str = "validation error: missing verification material";

/// A signed entity (verify.SignedEntity): a bundle, or the policy helpers' hashed record
/// of a cosign signature.
#[derive(Debug)]
pub struct Entity {
    /// Version().
    pub version: String,
    /// TlogEntries().
    pub entries: Result<Vec<tlog::Entry>, String>,
    /// Timestamps().
    pub timestamps: Result<Vec<Vec<u8>>, String>,
    /// VerificationContent().
    content: Result<Content, String>,
    /// SignatureContent().
    pub signature: Result<SignatureContent, String>,
}

impl Entity {
    /// The bundle as sigstore-go's Bundle entity is.
    pub fn from_bundle(b: &Bundle) -> Entity {
        Entity {
            version: b.version.clone(),
            entries: Ok(b.entries.clone()),
            timestamps: b.signed_timestamps().map_err(|e| e.0),
            content: verification_content(b),
            signature: b.signature_content().map_err(|e| e.0),
        }
    }

    /// A hashed record: its signature over the digest, its certificate (or, for DHI,
    /// the key material's key, hinted ""), its log entries.
    pub fn hashed_record(
        digest: Vec<u8>,
        algorithm: String,
        signature: Vec<u8>,
        certificate: Option<Certificate>,
        entries: Result<Vec<tlog::Entry>, String>,
    ) -> Entity {
        Entity {
            version: "v0.1".into(),
            entries,
            timestamps: Ok(Vec::new()),
            content: Ok(match certificate {
                Some(c) => Content::Certificate(Box::new(c)),
                None => Content::Key(String::new()),
            }),
            signature: Ok(SignatureContent::Message {
                digest,
                algorithm,
                signature,
            }),
        }
    }
}

/// Bundle.VerificationContent.
fn verification_content(b: &Bundle) -> Result<Content, String> {
    let parse = |raw: &[u8]| {
        Certificate::parse(raw)
            .map(|c| Content::Certificate(Box::new(c)))
            .map_err(|e| format!("validation error: {e}"))
    };
    match &b.material {
        BundleMaterial::Chain(certs) => match certs.first() {
            Some(Some(raw)) => parse(raw),
            _ => Err(MISSING_MATERIAL.into()),
        },
        BundleMaterial::Certificate(Some(raw)) => parse(raw),
        BundleMaterial::PublicKey { hint } => Ok(Content::Key(hint.clone())),
        _ => Err(MISSING_MATERIAL.into()),
    }
}

impl Material<'_> {
    /// PublicKeyVerifier: the key material's verifier, whatever the hint.
    fn key_verifier(&self) -> Result<&KeyMaterial, String> {
        self.key
            .ok_or_else(|| "public key verifier not found".to_string())
    }

    fn cas(&self) -> &[crate::trusted_root::CertificateAuthority] {
        if self.fulcio {
            &self.root.certificate_authorities
        } else {
            &[]
        }
    }

    fn ctlogs(&self) -> std::collections::BTreeMap<String, crate::trusted_root::TransparencyLog> {
        if self.fulcio {
            self.root.ctlogs.clone()
        } else {
            Default::default()
        }
    }
}

impl Content {
    /// ValidAtTime.
    fn valid_at(&self, t: &Time, material: &Material<'_>) -> bool {
        match self {
            Content::Certificate(c) => !(c.not_after < *t) && !(c.not_before > *t),
            Content::Key(_) => material.key_verifier().is_ok_and(|k| t.secs >= k.valid_from),
        }
    }

    /// CompareKey.
    fn compare_key(&self, key: Option<&EntryKey>, material: &Material<'_>) -> bool {
        match (self, key) {
            (Content::Certificate(c), Some(EntryKey::Certificate(e))) => c.raw == e.raw,
            (Content::Key(_), Some(EntryKey::Key(k))) => material
                .key_verifier()
                .is_ok_and(|v| v.verifier.public_key() == *k),
            _ => false,
        }
    }
}

/// VerifyTlogEntry.
fn verify_tlog_entries(
    b: &Entity,
    material: &Material<'_>,
    threshold: usize,
    trust_integrated: bool,
    zone: Zone,
) -> Result<Vec<VerifiedTimestamp>, String> {
    let entries = b.entries.as_ref().map_err(Clone::clone)?;
    if entries.len() > crate::bundle::MAX_TLOG_ENTRIES {
        return Err(format!(
            "too many tlog entries: {} > {}",
            entries.len(),
            crate::bundle::MAX_TLOG_ENTRIES
        ));
    }
    for (i, a) in entries.iter().enumerate() {
        for c in entries.iter().skip(i + 1) {
            if a.log_key_id() == c.log_key_id() && a.log_index() == c.log_index() {
                return Err("duplicate tlog entries found".into());
            }
        }
    }
    let sig_content = b.signature.clone()?;
    let entity_signature = sig_content.signature();
    let content = b.content.as_ref().map_err(Clone::clone)?;
    let logs = &material.root.tlogs;
    let mut verified: Vec<VerifiedTimestamp> = Vec::new();
    let mut verified_ids: Vec<Vec<u8>> = Vec::new();
    let mut timestamped: Vec<Vec<u8>> = Vec::new();
    for entry in entries {
        tlog::validate_entry(entry)?;
        let key_id = entry.log_key_id();
        let hex: String = key_id.iter().map(|x| format!("{x:02x}")).collect();
        let Some(log) = logs.get(&hex) else {
            continue;
        };
        if !entry.has_inclusion_promise() && !entry.has_inclusion_proof() {
            return Err("entry must contain an inclusion proof and/or promise".into());
        }
        if entry.is_rekor_v2() && !entry.has_inclusion_proof() {
            return Err("rekor v2 entries must have an inclusion proof".into());
        }
        if entry.has_inclusion_promise() && tlog::verify_set(entry, logs, zone).is_err() {
            continue;
        }
        if entry.has_inclusion_proof() {
            let verifier = keys::load(
                &log.key,
                keys::Load {
                    hash: Some(Some(log.signature_hash)),
                    ..keys::Load::default()
                },
            )?;
            if tlog::has_rekor_v1_sth(entry) {
                tlog::verify_inclusion_v1(entry, &verifier)?;
            } else {
                if log.base_url.is_empty() {
                    return Err(
                        "cannot verify Rekor v2 entry without baseUrl in transparency log's trusted root"
                            .into(),
                    );
                }
                let url = shards_dockerfile::url::parse(log.base_url.as_bytes())
                    .map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
                let host = url_hostname(&url.host);
                let hash = v2_entry_hash(&sig_content, content, material, &entity_signature)?;
                tlog::verify_v2(entry, &host, &verifier, &hash)?;
            }
        }
        if !entry.is_rekor_v2() {
            if entry.signature() != entity_signature {
                return Err("transparency log signature does not match".into());
            }
            if !content.compare_key(entry.public_key().as_ref(), material) {
                return Err("transparency log certificate does not match".into());
            }
            match &sig_content {
                SignatureContent::Message {
                    digest, algorithm, ..
                } => {
                    let Some((entry_digest, entry_alg)) = entry.hashed_rekord_digest() else {
                        return Err("transparency log entry is not a hashedrekord or missing digest".into());
                    };
                    let a = signature::message_hash(algorithm)?;
                    let e = signature::message_hash(&entry_alg)?;
                    if a != e {
                        return Err(format!(
                            "transparency log hashedrekord entry digest algorithm mismatch: {algorithm} != {entry_alg}"
                        ));
                    }
                    if entry_digest != *digest {
                        return Err(format!(
                            "transparency log hashedrekord entry digest {} does not match artifact {}",
                            hex_of(&entry_digest),
                            hex_of(digest)
                        ));
                    }
                }
                SignatureContent::Envelope(env) => {
                    let payload =
                        crate::gobase64::decode(env.payload.as_bytes(), false, true).map_err(|o| {
                            format!(
                                "failed to decode envelope payload: {}",
                                crate::gobase64::error_text(o)
                            )
                        })?;
                    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &payload);
                    let Some(entry_digest) = entry.dsse_payload_hash() else {
                        return Err(
                            "transparency log rekor v1 entry is not a dsse_v001 or intoto_v002 entry".into(),
                        );
                    };
                    if hash.as_ref() != entry_digest.as_slice() {
                        return Err(format!(
                            "transparency log dsse/intoto entry payload hash {} does not match envelope payload hash {}",
                            hex_of(hash.as_ref()),
                            hex_of(&entry_digest)
                        ));
                    }
                }
            }
        }
        let integrated = entry.integrated_time(zone);
        if !integrated.is_zero() && !content.valid_at(&integrated, material) {
            return Err("integrated time outside certificate validity".into());
        }
        if !verified_ids.contains(&key_id) {
            verified_ids.push(key_id.clone());
        }
        if trust_integrated && entry.has_inclusion_promise() && !timestamped.contains(&key_id) {
            timestamped.push(key_id.clone());
            verified.push(VerifiedTimestamp {
                kind: "Tlog",
                uri: log.base_url.clone(),
                time: integrated,
            });
        }
    }
    if verified_ids.len() < threshold {
        return Err(format!(
            "not enough verified log entries from transparency log: {} < {threshold}",
            verified_ids.len()
        ));
    }
    Ok(verified)
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// url.URL.Hostname: the host without its port, brackets removed.
fn url_hostname(host: &[u8]) -> String {
    let h = String::from_utf8_lossy(host).into_owned();
    if let Some(rest) = h.strip_prefix('[')
        && let Some(i) = rest.find(']')
    {
        return rest.get(..i).unwrap_or_default().to_string();
    }
    match h.rfind(':') {
        Some(i)
            if h.get(i + 1..)
                .is_some_and(|p| p.bytes().all(|c| c.is_ascii_digit())) =>
        {
            h.get(..i).unwrap_or_default().to_string()
        }
        _ => h,
    }
}

/// reconstructV2EntryHash.
fn v2_entry_hash(
    sig: &SignatureContent,
    content: &Content,
    material: &Material<'_>,
    entity_signature: &[u8],
) -> Result<Vec<u8>, String> {
    let (key, verifier) = match content {
        Content::Certificate(c) => (c.public_key.clone(), tlog::V2Verifier::Certificate(c.raw.clone())),
        Content::Key(_) => {
            let v = material
                .key_verifier()
                .map_err(|e| format!("public key not found in trusted material: {e}"))?;
            let k = v.verifier.public_key();
            let raw = keys::marshal_pkix(&k)
                .ok_or_else(|| "marshaling public key: x509: unsupported public key type".to_string())?;
            (k, tlog::V2Verifier::PublicKey(raw))
        }
    };
    let details = keys::default_details(&key, true, false)
        .map_err(|e| format!("getting algorithm details from bundle key: {e}"))?;
    let Some(hash) = details.hash() else {
        return Err("rekor v2 hashedrekord entries require a prehashing signature algorithm".into());
    };
    let digest = match sig {
        SignatureContent::Message { digest, .. } => digest.clone(),
        SignatureContent::Envelope(env) => {
            let payload = crate::gobase64::decode(env.payload.as_bytes(), false, true).map_err(|o| {
                format!(
                    "failed to decode envelope payload: {}",
                    crate::gobase64::error_text(o)
                )
            })?;
            hash.of(&signature::pae(&env.payload_type, &payload))
        }
    };
    tlog::v2_entry_hash(&digest, entity_signature, &verifier, details)
}

/// VerifySignedTimestamp: the timestamps verified, and each failure.
fn signed_timestamps(
    b: &Entity,
    material: &Material<'_>,
) -> Result<(Vec<VerifiedTimestamp>, Vec<String>), String> {
    let stamps = b.timestamps.clone()?;
    if stamps.len() > 32 {
        return Err(format!("too many signed timestamps: {} > 32", stamps.len()));
    }
    let sig = b.signature.clone()?.signature();
    let mut verified: Vec<VerifiedTimestamp> = Vec::new();
    let mut errors = Vec::new();
    for st in &stamps {
        let mut errs = Vec::new();
        let mut found = None;
        for tsa in &material.root.timestamp_authorities {
            match crate::tsa::verify(tsa, st, &sig) {
                Ok(t) => {
                    found = Some(t);
                    break;
                }
                Err(e) => errs.push(e),
            }
        }
        let Some(t) = found else {
            errors.push(format!("unable to verify signed timestamps: {}", errs.join("\n")));
            continue;
        };
        if verified.iter().any(|v| v.uri == t.uri) {
            errors.push(format!(
                "duplicate timestamps from the same authority, ignoring {}",
                t.uri
            ));
            continue;
        }
        verified.push(VerifiedTimestamp {
            kind: "TimestampAuthority",
            uri: t.uri,
            time: t.time,
        });
    }
    Ok((verified, errors))
}

/// %w of errors.Join(errs...): the errors a line each, or what fmt prints for nil.
fn joined(errs: &[String]) -> String {
    if errs.is_empty() {
        "%!w(<nil>)".into()
    } else {
        errs.join("\n")
    }
}

/// VerifyObserverTimestamps.
fn observer_timestamps(
    b: &Entity,
    material: &Material<'_>,
    config: &Config,
    logged: Vec<VerifiedTimestamp>,
) -> Result<Vec<VerifiedTimestamp>, String> {
    let mut out = Vec::new();
    if config.signed > 0 {
        let (v, errors) = signed_timestamps(b, material)?;
        if v.len() < config.signed {
            return Err(format!(
                "threshold not met for verified signed timestamps: {} < {}; error: {}",
                v.len(),
                config.signed,
                joined(&errors)
            ));
        }
        out.extend(v);
    }
    if config.integrated > 0 {
        if logged.len() < config.integrated {
            return Err(format!(
                "threshold not met for verified log entry integrated timestamps: {} < {}",
                logged.len(),
                config.integrated
            ));
        }
        out.extend(logged.iter().cloned());
    }
    if config.observer > 0 {
        let (v, errors) =
            signed_timestamps(b, material).map_err(|e| format!("failed to verify signed timestamps: {e}"))?;
        let n = v.len() + logged.len();
        if n < config.observer {
            return Err(format!(
                "threshold not met for verified signed & log entry integrated timestamps: {n} < {}; error: {}",
                config.observer,
                joined(&errors)
            ));
        }
        out.extend(logged.iter().cloned());
        out.extend(v);
    }
    if out.is_empty() && !config.no_observer {
        return Err("no valid observer timestamps found".into());
    }
    Ok(out)
}

/// verifyLeafCertificate: the first CA's chains for the leaf at `at`.
fn leaf_chains<'c>(
    leaf: &'c Certificate,
    at: &Time,
    pools: &'c [(crate::x509::Pool, crate::x509::Pool, Option<Time>, Option<Time>)],
) -> Result<Vec<Vec<&'c Certificate>>, String> {
    for (roots, inter, start, end) in pools {
        if start.as_ref().is_some_and(|s| !s.is_zero() && at < s)
            || end.as_ref().is_some_and(|e| !e.is_zero() && at > e)
        {
            continue;
        }
        let opts = crate::x509::Options {
            roots,
            intermediates: inter,
            now: *at,
            key_usages: vec![crate::x509::Eku::CodeSigning],
        };
        if let Ok(chains) = leaf.verify(&opts) {
            return Ok(chains);
        }
    }
    Err("leaf certificate verification failed".into())
}

/// Verify: the bundle `bundle` (its JSON), against `material`, as `config` and `policy`
/// ask, at `now`, its times printed in `zone`.
pub fn verify(
    bundle: &[u8],
    material: &Material<'_>,
    config: &Config,
    policy: &Policy,
    zone: Zone,
    now: Time,
) -> Result<Outcome, Error> {
    let _ = now;
    let b = crate::bundle::parse(bundle)?;
    verify_entity(&Entity::from_bundle(&b), material, config, policy, zone)
}

/// Verifier.Verify of a signed entity.
pub fn verify_entity(
    b: &Entity,
    material: &Material<'_>,
    config: &Config,
    policy: &Policy,
    zone: Zone,
) -> Result<Outcome, Error> {
    validate(config)?;
    let fail = |what: &str, e: String| Error(format!("{what}: {e}"));
    let logged = if config.tlog > 0 {
        verify_tlog_entries(
            b,
            material,
            config.tlog,
            config.integrated > 0 || config.observer > 0,
            zone,
        )
        .map_err(|e| fail("failed to verify log inclusion", e))?
    } else {
        Vec::new()
    };
    let timestamps = observer_timestamps(b, material, config, logged)
        .map_err(|e| fail("failed to verify timestamps", e))?;
    let content = b
        .content
        .as_ref()
        .map_err(|e| fail("failed to fetch verification content", e.clone()))?;
    let mut outcome = Outcome::default();
    match content {
        Content::Certificate(leaf) => {
            if config.no_observer {
                return Err(Error("must provide timestamp to verify certificate".into()));
            }
            let summary =
                crate::summary::summarize(leaf).map_err(|e| fail("failed to summarize certificate", e))?;
            let mut leaf = (**leaf).clone();
            leaf.unhandled_critical.retain(|o| o != crate::x509::OID_SAN);
            let pools: Vec<_> = material
                .cas()
                .iter()
                .map(|ca| {
                    let mut roots = crate::x509::Pool::default();
                    roots.add(ca.root.clone());
                    let mut inter = crate::x509::Pool::default();
                    for c in &ca.intermediates {
                        inter.add(c.clone());
                    }
                    (roots, inter, ca.start, ca.end)
                })
                .collect();
            let mut chains = Vec::new();
            for ts in &timestamps {
                chains = leaf_chains(&leaf, &ts.time, &pools)
                    .map_err(|e| fail("failed to verify leaf certificate", e))?;
            }
            if config.sct > 0 {
                if chains.first().and_then(|c| c.first()).is_none() {
                    return Err(fail(
                        "failed to verify signed certificate timestamp",
                        "no chains provided".into(),
                    ));
                }
                crate::sct::verify_scts(&chains, config.sct, &material.ctlogs())
                    .map_err(|e| fail("failed to verify signed certificate timestamp", e))?;
            }
            outcome.certificate = Some(summary);
        }
        Content::Key(_) => {
            for ts in &timestamps {
                if !content.valid_at(&ts.time, material) {
                    return Err(Error(
                        "signature time outside of public key validity window".into(),
                    ));
                }
            }
            if config.sct > 0 {
                return Err(Error(
                    "SCTs required but bundle is signed with a public key, which cannot contain SCTs".into(),
                ));
            }
        }
    }
    let sig = b
        .signature
        .clone()
        .map_err(|e| fail("failed to fetch signature content", e))?;
    let compat = matches!(
        b.version.as_str(),
        "v0.1" | "0.1" | "v0.2" | "0.2" | "v0.3" | "0.3"
    );
    let verifier = match content {
        Content::Certificate(leaf) => signature::compat_verifier(
            &leaf.public_key,
            compat,
            matches!(sig, SignatureContent::Envelope(_)),
        ),
        Content::Key(_) => material
            .key_verifier()
            .map(|k| signature::SigVerifier::Single(k.verifier.clone())),
    }
    .map_err(|e| fail("failed to get signature verifier", e))?;
    match &policy.digest {
        Some((alg, d)) => signature::verify_with_digest(&verifier, &sig, alg, d),
        // WithoutArtifactUnsafe.
        None => signature::verify_without_artifact(&verifier, &sig),
    }
    .map_err(|e| fail("failed to verify signature", e))?;
    if let Content::Key(hint) = content {
        outcome.public_key_id = Some(hint.clone());
    }
    if let SignatureContent::Envelope(env) = &sig {
        outcome.statement =
            Some(signature::statement(env).map_err(|e| fail("failed to fetch envelope statement", e))?);
    }
    outcome.timestamps = timestamps;
    if policy.identity == Identity::Any && outcome.certificate.is_none() {
        return Err(Error(
            "can't verify certificate identities: entity was not signed with a certificate".into(),
        ));
    }
    Ok(outcome)
}
