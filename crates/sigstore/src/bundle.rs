//! Sigstore bundles (bundle/v1 Bundle) as protojson reads them and sigstore-go's bundle
//! package holds them.

/// A transparency log entry as the bundle gives it (rekor/v1 TransparencyLogEntry):
/// each message field present or not, as protobuf-go holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlogEntry {
    pub log_index: i64,
    /// logId, and its keyId where set (an empty one is unset).
    pub log_id: Option<Option<Vec<u8>>>,
    /// kindVersion: (kind, version).
    pub kind_version: Option<(String, String)>,
    pub integrated_time: i64,
    /// inclusionPromise, and its signedEntryTimestamp where set.
    pub inclusion_promise: Option<Option<Vec<u8>>>,
    pub inclusion_proof: Option<InclusionProof>,
    /// canonicalizedBody; an empty one is unset.
    pub canonicalized_body: Option<Vec<u8>>,
}

/// rekor/v1 InclusionProof.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InclusionProof {
    pub log_index: i64,
    pub root_hash: Vec<u8>,
    pub tree_size: i64,
    pub hashes: Vec<Vec<u8>>,
    /// checkpoint, and its envelope.
    pub checkpoint: Option<String>,
}

use crate::Error;
use crate::proto::{self, Msg, Val};
use crate::schemas;

const MEDIA_TYPE_BASE: &str = "application/vnd.dev.sigstore.bundle";
const ERR_VALIDATION: &str = "validation error";
const MISSING_MATERIAL: &str = "validation error: missing verification material";
const MISSING_ENVELOPE: &str = "validation error: invalid attestation: missing valid envelope";
/// MaxAllowedTlogEntries.
pub const MAX_TLOG_ENTRIES: usize = 32;

/// The bundle's verification material (VerificationMaterial.content), each as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Material {
    None,
    PublicKey {
        hint: String,
    },
    /// X509CertificateChain: present, and its certificates' raw bytes (None where unset).
    Chain(Vec<Option<Vec<u8>>>),
    /// X509Certificate's raw bytes (None where unset).
    Certificate(Option<Vec<u8>>),
}

/// A DSSE envelope as sigstore-go re-encodes it (dsse.Envelope): payload and signatures
/// in standard base64.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    pub payload: String,
    pub payload_type: String,
    /// (keyid, sig).
    pub signatures: Vec<(String, String)>,
}

/// The bundle's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    None,
    /// MessageSignature: present, its digest (algorithm, digest) where set, and its
    /// signature.
    Message {
        digest: Option<(i32, Option<Vec<u8>>)>,
        signature: Option<Vec<u8>>,
    },
    /// The protobuf envelope: present, its payload where set, payloadType, signatures
    /// (sig where set, keyid).
    Envelope {
        payload: Option<Vec<u8>>,
        payload_type: String,
        signatures: Vec<(Option<Vec<u8>>, String)>,
    },
}

/// A bundle as loadBundle loads it: unmarshalled and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub media_type: String,
    /// Version(): `v0.3`.
    pub version: String,
    /// verificationMaterial present.
    pub has_material: bool,
    pub material: Material,
    pub tlog_entries: Vec<TlogEntry>,
    pub entries: Vec<crate::tlog::Entry>,
    /// timestampVerificationData: present, and each signedTimestamp (None where unset).
    pub timestamps: Option<Vec<Option<Vec<u8>>>>,
    pub content: Content,
}

/// getBundleVersion.
pub fn bundle_version(media_type: &str) -> Result<String, Error> {
    let fixed = [
        (format!("{MEDIA_TYPE_BASE}+json;version=0.1"), "v0.1"),
        (format!("{MEDIA_TYPE_BASE}+json;version=0.2"), "v0.2"),
        (format!("{MEDIA_TYPE_BASE}+json;version=0.3"), "v0.3"),
    ];
    if let Some((_, v)) = fixed.iter().find(|(m, _)| m == media_type) {
        return Ok((*v).to_string());
    }
    let prefix = format!("{MEDIA_TYPE_BASE}.v");
    if media_type.starts_with(&prefix) && media_type.ends_with("+json") {
        let version = media_type
            .strip_prefix(&format!("{MEDIA_TYPE_BASE}."))
            .unwrap_or(media_type);
        let version = version.strip_suffix("+json").unwrap_or(version);
        if crate::semver::is_valid(version) {
            return Ok(version.to_string());
        }
        return Err(Error(format!(
            "{ERR_VALIDATION}: unsupported media type: invalid bundle version: {version}"
        )));
    }
    Err(Error(format!(
        "{ERR_VALIDATION}: unsupported media type: {media_type}"
    )))
}

fn msgs(list: &[Val]) -> Vec<&Msg> {
    list.iter()
        .filter_map(|v| match v {
            Val::Msg(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn tlog_entry(m: &Msg) -> TlogEntry {
    TlogEntry {
        log_index: m.int(1),
        log_id: m.msg(2).map(|l| l.bytes(1)),
        kind_version: m.msg(3).map(|k| (k.string(1), k.string(2))),
        integrated_time: m.int(4),
        inclusion_promise: m.msg(5).map(|p| p.bytes(1)),
        inclusion_proof: m.msg(6).map(|p| InclusionProof {
            log_index: p.int(1),
            root_hash: p.bytes(2).unwrap_or_default(),
            tree_size: p.int(3),
            hashes: p
                .list(4)
                .iter()
                .map(|h| match h {
                    Val::Bytes(b) => b.clone(),
                    _ => Vec::new(),
                })
                .collect(),
            checkpoint: p.msg(5).map(|c| c.string(1)),
        }),
        canonicalized_body: m.bytes(7),
    }
}

/// Bundle.UnmarshalJSON: protojson, then validate().
pub fn parse(json: &[u8]) -> Result<Bundle, Error> {
    let m = proto::unmarshal(json, &schemas::BUNDLE).map_err(|e| Error(e.to_string()))?;
    let vm = m.msg(2);
    let material = match vm {
        Some(vm) => {
            if let Some(pk) = vm.msg(1) {
                Material::PublicKey { hint: pk.string(1) }
            } else if let Some(chain) = vm.msg(2) {
                Material::Chain(msgs(chain.list(1)).iter().map(|c| c.bytes(1)).collect())
            } else if let Some(c) = vm.msg(5) {
                Material::Certificate(c.bytes(1))
            } else {
                Material::None
            }
        }
        None => Material::None,
    };
    let tlog_entries: Vec<TlogEntry> = vm
        .map(|vm| msgs(vm.list(3)).into_iter().map(tlog_entry).collect())
        .unwrap_or_default();
    let timestamps = vm
        .and_then(|vm| vm.msg(4))
        .map(|t| msgs(t.list(1)).iter().map(|s| s.bytes(1)).collect());
    let content = if let Some(ms) = m.msg(3) {
        Content::Message {
            digest: ms.msg(1).map(|d| (d.enumeration(1), d.bytes(2))),
            signature: ms.bytes(2),
        }
    } else if let Some(env) = m.msg(4) {
        Content::Envelope {
            payload: env.bytes(1),
            payload_type: env.string(2),
            signatures: msgs(env.list(3))
                .iter()
                .map(|s| (s.bytes(1), s.string(2)))
                .collect(),
        }
    } else {
        Content::None
    };
    let mut b = Bundle {
        media_type: m.string(1),
        version: String::new(),
        has_material: vm.is_some(),
        material,
        tlog_entries,
        entries: Vec::new(),
        timestamps,
        content,
    };
    b.validate()?;
    Ok(b)
}

impl Bundle {
    /// TlogEntries: each entry parsed (ParseTransparencyLogEntry).
    fn tlog_entries(&self) -> Result<Vec<crate::tlog::Entry>, Error> {
        if !self.has_material {
            return Ok(Vec::new());
        }
        let n = self.tlog_entries.len();
        if n > MAX_TLOG_ENTRIES {
            return Err(Error(format!(
                "{ERR_VALIDATION}: too many tlog entries: {n} > {MAX_TLOG_ENTRIES}"
            )));
        }
        let mut out = Vec::with_capacity(n);
        for e in &self.tlog_entries {
            out.push(crate::tlog::parse_entry(e).map_err(|e| Error(format!("{ERR_VALIDATION}: {e}")))?);
        }
        Ok(out)
    }

    /// validate.
    fn validate(&mut self) -> Result<(), Error> {
        use std::cmp::Ordering;
        let version = bundle_version(&self.media_type)
            .map_err(|e| Error(format!("error getting bundle version: {e}")))?;
        if crate::semver::compare(&version, "v0.1") == Ordering::Less {
            return Err(Error(format!(
                "{ERR_VALIDATION}: unsupported media type: bundle version {version} is not supported"
            )));
        }
        let entries = self.tlog_entries()?;
        let promise = entries.iter().any(crate::tlog::Entry::has_inclusion_promise);
        let proof = entries.iter().any(crate::tlog::Entry::has_inclusion_proof);
        if crate::semver::compare(&version, "v0.1") == Ordering::Equal {
            if !entries.is_empty() && !promise {
                return Err(Error(
                    "inclusion promises missing in bundle (required for bundle v0.1)".into(),
                ));
            }
        } else if !entries.is_empty() && !proof {
            return Err(Error(
                "inclusion proof missing in bundle (required for bundle v0.2)".into(),
            ));
        }
        if crate::semver::compare(&version, "v0.3") != Ordering::Less
            && matches!(self.material, Material::Chain(_))
        {
            return Err(Error(
                "verification material cannot be X.509 certificate chain (for bundle v0.3)".into(),
            ));
        }
        if crate::semver::compare(&version, "v0.4") != Ordering::Less {
            return Err(Error(format!(
                "{ERR_VALIDATION}: unsupported media type: bundle version {version} is not yet supported"
            )));
        }
        if self.content == Content::None {
            return Err(Error(format!(
                "invalid bundle: {ERR_VALIDATION}: missing bundle content"
            )));
        }
        if !self.has_material || self.material == Material::None {
            return Err(Error(format!("invalid bundle: {MISSING_MATERIAL}")));
        }
        self.version = version;
        self.entries = entries;
        Ok(())
    }

    /// RFC 3161 timestamps (Timestamps).
    pub fn signed_timestamps(&self) -> Result<Vec<Vec<u8>>, Error> {
        if !self.has_material {
            return Err(Error(MISSING_MATERIAL.into()));
        }
        Ok(self
            .timestamps
            .as_ref()
            .map(|t| t.iter().map(|s| s.clone().unwrap_or_default()).collect())
            .unwrap_or_default())
    }

    /// SignatureContent.
    pub fn signature_content(&self) -> Result<SignatureContent, Error> {
        match &self.content {
            Content::Envelope {
                payload,
                payload_type,
                signatures,
            } => {
                use base64::Engine as _;
                let Some(payload) = payload else {
                    return Err(Error(MISSING_ENVELOPE.into()));
                };
                let b64 = base64::engine::general_purpose::STANDARD;
                Ok(SignatureContent::Envelope(Envelope {
                    payload: b64.encode(payload),
                    payload_type: payload_type.clone(),
                    signatures: signatures
                        .iter()
                        .map(|(sig, keyid)| (keyid.clone(), b64.encode(sig.clone().unwrap_or_default())))
                        .collect(),
                }))
            }
            Content::Message { digest, signature } => {
                let Some((alg, d)) = digest else {
                    return Err(Error(MISSING_MATERIAL.into()));
                };
                // HashAlgorithm_name[alg]: an unknown number's name is "".
                let name = schemas::HASH_ALGORITHM
                    .iter()
                    .find(|(_, n)| n == alg)
                    .map(|(name, _)| (*name).to_string())
                    .unwrap_or_default();
                Ok(SignatureContent::Message {
                    digest: d.clone().unwrap_or_default(),
                    algorithm: name,
                    signature: signature.clone().unwrap_or_default(),
                })
            }
            Content::None => Err(Error(MISSING_MATERIAL.into())),
        }
    }
}

/// SignatureContent: a message signature or an envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureContent {
    Message {
        digest: Vec<u8>,
        algorithm: String,
        signature: Vec<u8>,
    },
    Envelope(Envelope),
}

impl SignatureContent {
    /// Signature(): the message's, or the envelope's first decoded (empty where it
    /// cannot be).
    pub fn signature(&self) -> Vec<u8> {
        match self {
            SignatureContent::Message { signature, .. } => signature.clone(),
            SignatureContent::Envelope(e) => e
                .signatures
                .first()
                .and_then(|(_, sig)| crate::gobase64::decode(sig.as_bytes(), false, true).ok())
                .unwrap_or_default(),
        }
    }
}
