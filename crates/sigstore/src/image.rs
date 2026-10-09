//! An image's signature as BuildKit's policy helpers find and verify it (moby/policy-helpers,
//! as buildx v0.37.1 vendors it): the chain from an index to its image manifest, the
//! attestation manifest beside it and the signature manifest referring to that
//! (image/resolve.go `ResolveSignatureChain`), and the signature verified
//! (verifier.go `VerifyImage`): a Sigstore bundle, or cosign's simple signing as a hashed
//! record (hashedrecordbundle.go), with Docker Hardened Images' own key (roots/dhi).
//! OCI documents are read as Go's encoding/json reads them into image-spec's types.

use std::collections::BTreeMap;

use crate::Error;
use crate::godec::{Any, Dec, Elem, GoSlice, anonymous};
use crate::helpers::Root;
use crate::helpers::{Kind, SignatureInfo, SignatureType};
use crate::platforms::{self, Only, Platform};
use crate::time::Zone;
use crate::tlog::gojson::{self, JValue};
use crate::verify::{self, Config, Entity, Identity, KeyMaterial, Material, Policy};

pub const MEDIA_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MEDIA_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const ARTIFACT_COSIGN_SIGNATURE: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";
pub const ARTIFACT_SIGSTORE_BUNDLE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
pub const ARTIFACT_IN_TOTO: &str = "application/vnd.in-toto+json";
pub const MEDIA_SIMPLE_SIGNING: &str = "application/vnd.dev.cosign.simplesigning.v1+json";
pub const SLSA_V02: &str = "https://slsa.dev/provenance/v0.2";
pub const SLSA_V1: &str = "https://slsa.dev/provenance/v1";
const PREDICATE_TYPE: &str = "in-toto.io/predicate-type";

/// image-spec's Descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: i64,
    pub urls: Vec<String>,
    pub annotations: BTreeMap<String, String>,
    pub data: Vec<u8>,
    pub platform: Option<Platform>,
    pub artifact_type: String,
}

/// image-spec's Index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    pub media_type: String,
    pub artifact_type: String,
    pub manifests: Vec<Descriptor>,
    pub subject: Option<Descriptor>,
    pub annotations: BTreeMap<String, String>,
}

/// image-spec's Manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub media_type: String,
    pub artifact_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    pub subject: Option<Descriptor>,
    pub annotations: BTreeMap<String, String>,
}

/// A Descriptor as Go holds it while decoding into it.
#[derive(Debug, Clone, Default)]
struct GoDescriptor {
    media_type: String,
    digest: String,
    size: i64,
    urls: GoSlice<String>,
    annotations: BTreeMap<String, String>,
    data: GoSlice<u8>,
    platform: Option<GoPlatform>,
    artifact_type: String,
}

#[derive(Debug, Clone, Default)]
struct GoPlatform {
    architecture: String,
    os: String,
    os_version: String,
    os_features: GoSlice<String>,
    variant: String,
}

/// v1.Descriptor's size on a 64-bit Go.
const DESCRIPTOR: Elem = Elem {
    size: 120,
    noscan: false,
};

impl GoDescriptor {
    fn into_descriptor(self) -> Descriptor {
        Descriptor {
            media_type: self.media_type,
            digest: self.digest,
            size: self.size,
            urls: self.urls.into_vec(),
            annotations: self.annotations,
            data: self.data.into_vec(),
            platform: self.platform.map(|p| Platform {
                architecture: p.architecture,
                os: p.os,
                os_version: p.os_version,
                os_features: p.os_features.into_vec(),
                variant: p.variant,
            }),
            artifact_type: self.artifact_type,
        }
    }
}

fn descriptor(d: &mut Dec, v: &JValue, dst: &mut GoDescriptor) {
    let fields = [
        ("mediaType", "mediaType"),
        ("digest", "digest"),
        ("size", "size"),
        ("urls", "urls"),
        ("annotations", "annotations"),
        ("data", "data"),
        ("platform", "platform"),
        ("artifactType", "artifactType"),
    ];
    d.object(v, "Descriptor", "v1.Descriptor", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.media_type, "string"),
        1 => d.string(x, &mut dst.digest, "digest.Digest"),
        2 => d.int64(x, &mut dst.size, "int64"),
        3 => d.strings(x, &mut dst.urls),
        4 => d.map(x, &mut dst.annotations),
        5 => d.bytes(x, &mut dst.data),
        6 => d.pointer(x, &mut dst.platform, platform),
        _ => d.string(x, &mut dst.artifact_type, "string"),
    });
}

fn platform(d: &mut Dec, v: &JValue, dst: &mut GoPlatform) {
    let fields = [
        ("architecture", "architecture"),
        ("os", "os"),
        ("os.version", "os.version"),
        ("os.features", "os.features"),
        ("variant", "variant"),
    ];
    d.object(v, "Platform", "v1.Platform", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.architecture, "string"),
        1 => d.string(x, &mut dst.os, "string"),
        2 => d.string(x, &mut dst.os_version, "string"),
        3 => d.strings(x, &mut dst.os_features),
        _ => d.string(x, &mut dst.variant, "string"),
    });
}

fn descriptors(d: &mut Dec, v: &JValue, dst: &mut GoSlice<GoDescriptor>) {
    d.slice(v, dst, "[]v1.Descriptor", DESCRIPTOR, descriptor);
}

/// json.Unmarshal into an Index.
pub fn parse_index(b: &[u8]) -> Result<Index, String> {
    let v = gojson::unmarshal(b)?;
    let mut d = Dec::new();
    let (mut media_type, mut artifact_type, mut annotations) =
        (String::new(), String::new(), BTreeMap::new());
    let (mut manifests, mut subject) = (GoSlice::default(), None);
    let fields = [
        ("schemaVersion", "Versioned.schemaVersion"),
        ("mediaType", "mediaType"),
        ("artifactType", "artifactType"),
        ("manifests", "manifests"),
        ("subject", "subject"),
        ("annotations", "annotations"),
    ];
    d.object(&v, "Index", "v1.Index", &fields, |d, i, x| match i {
        0 => d.int64(x, &mut 0, "int"),
        1 => d.string(x, &mut media_type, "string"),
        2 => d.string(x, &mut artifact_type, "string"),
        3 => descriptors(d, x, &mut manifests),
        4 => d.pointer(x, &mut subject, descriptor),
        _ => d.map(x, &mut annotations),
    });
    d.done(Index {
        media_type,
        artifact_type,
        manifests: manifests
            .into_vec()
            .into_iter()
            .map(GoDescriptor::into_descriptor)
            .collect(),
        subject: subject.map(GoDescriptor::into_descriptor),
        annotations,
    })
}

/// json.Unmarshal into a Manifest.
pub fn parse_manifest(b: &[u8]) -> Result<Manifest, String> {
    let v = gojson::unmarshal(b)?;
    let mut d = Dec::new();
    let (mut media_type, mut artifact_type, mut annotations) =
        (String::new(), String::new(), BTreeMap::new());
    let (mut config, mut layers, mut subject) = (GoDescriptor::default(), GoSlice::default(), None);
    let fields = [
        ("schemaVersion", "Versioned.schemaVersion"),
        ("mediaType", "mediaType"),
        ("artifactType", "artifactType"),
        ("config", "config"),
        ("layers", "layers"),
        ("subject", "subject"),
        ("annotations", "annotations"),
    ];
    d.object(&v, "Manifest", "v1.Manifest", &fields, |d, i, x| match i {
        0 => d.int64(x, &mut 0, "int"),
        1 => d.string(x, &mut media_type, "string"),
        2 => d.string(x, &mut artifact_type, "string"),
        3 => descriptor(d, x, &mut config),
        4 => descriptors(d, x, &mut layers),
        5 => d.pointer(x, &mut subject, descriptor),
        _ => d.map(x, &mut annotations),
    });
    d.done(Manifest {
        media_type,
        artifact_type,
        config: config.into_descriptor(),
        layers: layers
            .into_vec()
            .into_iter()
            .map(GoDescriptor::into_descriptor)
            .collect(),
        subject: subject.map(GoDescriptor::into_descriptor),
        annotations,
    })
}

/// What a signature chain is read from (image.ReferrersProvider): referrers of a digest,
/// and a blob's content.
pub trait Provider {
    /// FetchReferrers of `digest`, filtered by artifact type and query.
    fn referrers(
        &self,
        digest: &str,
        artifact_types: &[&str],
        filters: &[(&str, &str)],
    ) -> Result<Vec<Descriptor>, String>;
    /// content.ReadBlob: the blob `desc` names, whole.
    fn read(&self, desc: &Descriptor) -> Result<Vec<u8>, String>;
}

fn sha256_digest(b: &[u8]) -> String {
    let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b);
    format!(
        "sha256:{}",
        d.as_ref().iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// ReadBlob: the blob, checked against its digest. Go wraps a nil error where they
/// differ, so a mismatch reads as no content, as it does there.
pub fn read_blob(p: &dyn Provider, desc: &Descriptor) -> Result<Vec<u8>, String> {
    let dt = p
        .read(desc)
        .map_err(|e| format!("reading blob {}: {e}", desc.digest))?;
    if desc.digest != sha256_digest(&dt) {
        return Ok(Vec::new());
    }
    Ok(dt)
}

/// isDHIIndex.
fn is_dhi_index(idx: &Index) -> bool {
    for d in &idx.manifests {
        if d.annotations
            .get("com.docker.dhi.build.id")
            .is_none_or(String::is_empty)
        {
            return false;
        }
    }
    idx.annotations
        .get("org.opencontainers.image.title")
        .is_some_and(|t| t.starts_with("dhi/"))
}

/// %+v of a *ocispecs.Platform.
fn platform_go(p: &Platform) -> String {
    format!(
        "&{{Architecture:{} OS:{} OSVersion:{} OSFeatures:[{}] Variant:{}}}",
        p.architecture,
        p.os,
        p.os_version,
        p.os_features.join(" "),
        p.variant
    )
}

/// resolveImageManifest.
fn resolve_image_manifest(idx: &Index, platform: &Platform) -> Result<Descriptor, String> {
    let only = Only::new(platform);
    let mut descs: Vec<&Descriptor> = idx
        .manifests
        .iter()
        .filter(|d| d.media_type == MEDIA_MANIFEST || d.media_type == MEDIA_DOCKER_MANIFEST)
        .filter(|d| d.platform.as_ref().is_none_or(|p| only.matches(p)))
        .collect();
    descs.sort_by(|a, b| {
        use std::cmp::Ordering;
        match (&a.platform, &b.platform) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => {
                if only.less(x, y) {
                    Ordering::Less
                } else if only.less(y, x) {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }
        }
    });
    descs.first().map(|d| (*d).clone()).ok_or_else(|| {
        format!(
            "no manifest for platform {}: not found",
            platforms::format_all(platform)
        )
    })
}

/// SignatureChain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureChain {
    pub image_manifest: Descriptor,
    pub attestation_manifest: Option<Descriptor>,
    pub signature_manifest: Option<Descriptor>,
    pub dhi: bool,
}

/// ResolveSignatureChain.
pub fn resolve_signature_chain(
    p: &dyn Provider,
    desc: &Descriptor,
    platform: &Platform,
) -> Result<SignatureChain, String> {
    if desc.media_type != MEDIA_INDEX {
        return Err(format!(
            "expected image index descriptor, got {}",
            desc.media_type
        ));
    }
    let dt = read_blob(p, desc)?;
    let index = parse_index(&dt).map_err(|e| format!("unmarshaling image index: {e}"))?;
    let dhi = is_dhi_index(&index);
    let manifest = resolve_image_manifest(&index, platform).map_err(|e| {
        format!(
            "resolving image manifest for platform {}: {e}",
            platform_go(platform)
        )
    })?;
    let attestation = if dhi {
        let all = p
            .referrers(
                &manifest.digest,
                &[ARTIFACT_IN_TOTO],
                &[("predicateType", SLSA_V02), ("predicateType", SLSA_V1)],
            )
            .map_err(|e| format!("fetching referrers for manifest {}: {e}", manifest.digest))?;
        let refs: Vec<Descriptor> = all
            .into_iter()
            .filter(|r| {
                r.artifact_type == ARTIFACT_IN_TOTO
                    && matches!(
                        r.annotations.get(PREDICATE_TYPE).map(String::as_str),
                        Some(SLSA_V02 | SLSA_V1)
                    )
            })
            .collect();
        match refs.into_iter().next() {
            Some(r) => Some(r),
            None => {
                return Err(format!(
                    "no attestation referrers found for DHI manifest {}",
                    manifest.digest
                ));
            }
        }
    } else {
        index
            .manifests
            .iter()
            .find(|d| {
                d.annotations.get("vnd.docker.reference.type").map(String::as_str)
                    == Some("attestation-manifest")
                    && d.annotations.get("vnd.docker.reference.digest") == Some(&manifest.digest)
            })
            .cloned()
    };
    let mut chain = SignatureChain {
        image_manifest: manifest,
        attestation_manifest: None,
        signature_manifest: None,
        dhi,
    };
    let Some(att) = attestation else {
        return Ok(chain);
    };
    let all = p
        .referrers(&att.digest, &[], &[])
        .map_err(|e| format!("fetching referrers for attestation manifest {}: {e}", att.digest))?;
    chain.attestation_manifest = Some(att);
    let mut refs: Vec<Descriptor> = all
        .into_iter()
        .filter(|r| {
            r.artifact_type == ARTIFACT_SIGSTORE_BUNDLE || r.artifact_type == ARTIFACT_COSIGN_SIGNATURE
        })
        .collect();
    // Bundles first, the order otherwise kept.
    refs.sort_by_key(|r| r.artifact_type != ARTIFACT_SIGSTORE_BUNDLE);
    chain.signature_manifest = refs.into_iter().next();
    Ok(chain)
}

/// NoSigChainError.
fn no_chain(target: &str, has_attestation: bool) -> String {
    if has_attestation {
        format!("no signature found for image {target}")
    } else {
        format!("no provenance attestation found for image {target}")
    }
}

/// The DHI key (roots/dhi/dhi.pub) and the time it is trusted from (dhiEpoch).
pub fn dhi_key() -> Result<KeyMaterial, Error> {
    const PEM: &[u8] = include_bytes!("../roots/dhi.pub");
    let (block, _) = crate::tlog::pem::decode(PEM).ok_or_else(|| Error("DHI key: no PEM block".into()))?;
    let key = crate::tlog::pem::parse_pkix(&block.bytes).map_err(Error)?;
    let verifier = crate::keys::load(&key, crate::keys::Load::default()).map_err(Error)?;
    Ok(KeyMaterial {
        verifier,
        valid_from: 1_743_595_200,
    })
}

/// The digest's algorithm and raw value (rawDigest).
fn raw_digest(d: &str) -> Result<(String, Vec<u8>), String> {
    let (alg, hex) = d.split_once(':').unwrap_or(("", d));
    let raw = crate::signature::hex_decode(hex)
        .ok_or_else(|| format!("decoding digest {d}: {}", crate::signature::hex_error(hex)))?;
    Ok((alg.to_string(), raw))
}

/// The simple signing payload's fields VerifyImage reads: its docker-reference,
/// docker-manifest-digest and type, decoded into VerifyImage's anonymous struct.
fn simple_signing(dt: &[u8]) -> Result<(String, String, String), String> {
    let v = gojson::unmarshal(dt)?;
    let identity_ty = anonymous(&[("DockerReference", "string", "docker-reference")]);
    let image_ty = anonymous(&[("DockerManifestDigest", "string", "docker-manifest-digest")]);
    let critical_ty = anonymous(&[
        ("Identity", &identity_ty, "identity"),
        ("Image", &image_ty, "image"),
        ("Type", "string", "type"),
    ]);
    let payload_ty = anonymous(&[
        ("Critical", &critical_ty, "critical"),
        ("Optional", "map[string]interface {}", "optional"),
    ]);
    let mut d = Dec::new();
    let (mut reference, mut digest, mut ty) = (String::new(), String::new(), String::new());
    let top = [("critical", "critical"), ("optional", "optional")];
    let critical = [("identity", "identity"), ("image", "image"), ("type", "type")];
    d.object(&v, "", &payload_ty, &top, |d, i, x| {
        if i != 0 {
            return d.any_map(x);
        }
        d.object(x, "", &critical_ty, &critical, |d, j, y| match j {
            0 => d.object(
                y,
                "",
                &identity_ty,
                &[("docker-reference", "docker-reference")],
                |d, _, z| d.string(z, &mut reference, "string"),
            ),
            1 => d.object(
                y,
                "",
                &image_ty,
                &[("docker-manifest-digest", "docker-manifest-digest")],
                |d, _, z| d.string(z, &mut digest, "string"),
            ),
            _ => d.string(y, &mut ty, "string"),
        });
    });
    d.done((reference, digest, ty))
}

/// rekorBundle: body, SET, log ID (hex), integrated time, log index.
type RekorBundle = (Vec<u8>, Vec<u8>, String, i64, i64);

/// An entry of the newer bundle shape, as parseRekorBundle declares it.
#[derive(Debug, Clone, Default)]
struct NewEntry {
    log_index: Any,
    key_id: String,
    integrated_time: Any,
    set: GoSlice<u8>,
    body: GoSlice<u8>,
}

/// The entry's size on a 64-bit Go: two interfaces, a string, two byte slices.
const NEW_ENTRY: Elem = Elem {
    size: 96,
    noscan: false,
};

fn new_entry(d: &mut Dec, v: &JValue, e: &mut NewEntry) {
    let fields = [
        ("logIndex", "logIndex"),
        ("logId", "logId"),
        ("integratedTime", "integratedTime"),
        ("inclusionPromise", "inclusionPromise"),
        ("canonicalizedBody", "canonicalizedBody"),
    ];
    d.object(v, "", "", &fields, |d, i, x| match i {
        0 => d.any(x, &mut e.log_index),
        1 => d.object(x, "", "", &[("keyId", "keyId")], |d, _, y| {
            d.string(y, &mut e.key_id, "string")
        }),
        2 => d.any(x, &mut e.integrated_time),
        3 => d.object(
            x,
            "",
            "",
            &[("signedEntryTimestamp", "signedEntryTimestamp")],
            |d, _, y| d.bytes(y, &mut e.set),
        ),
        _ => d.bytes(x, &mut e.body),
    });
}

/// strings.ToLower: each rune's lower case, as unicode.ToLower maps it alone.
fn go_lower(s: &str) -> String {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// cosign's Rekor bundle annotation, read as parseRekorBundle reads it: the newer shape
/// where it decodes whole and its first entry has a body and a SET, else the older.
fn rekor_bundle(b: &[u8]) -> Result<RekorBundle, String> {
    if let Ok(v) = gojson::unmarshal(b) {
        let mut d = Dec::new();
        let mut entries: GoSlice<NewEntry> = GoSlice::default();
        d.object(&v, "", "", &[("content", "content")], |d, _, c| {
            d.object(
                c,
                "",
                "",
                &[("verificationMaterial", "verificationMaterial")],
                |d, _, m| {
                    d.object(m, "", "", &[("tlogEntries", "tlogEntries")], |d, _, t| {
                        d.slice(t, &mut entries, "", NEW_ENTRY, new_entry)
                    })
                },
            )
        });
        if d.ok()
            && let Some(e) = entries.items().first()
            && !e.body.items().is_empty()
            && !e.set.items().is_empty()
        {
            return Ok((
                e.body.items().to_vec(),
                e.set.items().to_vec(),
                go_lower(&e.key_id),
                e.integrated_time.to_int64().unwrap_or(0),
                e.log_index.to_int64().unwrap_or(0),
            ));
        }
    }
    let v = gojson::unmarshal(b).map_err(|e| format!("parse bundle json: {e}"))?;
    let payload_ty = anonymous(&[
        ("Body", "[]uint8", "body"),
        ("LogID", "interface {}", "logID"),
        ("IntegratedTime", "interface {}", "integratedTime"),
        ("LogIndex", "interface {}", "logIndex"),
    ]);
    let bundle_ty = anonymous(&[
        ("SignedEntryTimestamp", "[]uint8", "SignedEntryTimestamp"),
        ("Payload", &payload_ty, "Payload"),
        ("LogID", "interface {}", "logID"),
        ("IntegratedTime", "interface {}", "integratedTime"),
        ("LogIndex", "interface {}", "logIndex"),
    ]);
    let mut d = Dec::new();
    let (mut set, mut body) = (GoSlice::default(), GoSlice::default());
    let (mut log_id, mut it, mut li) = (Any::Nil, Any::Nil, Any::Nil);
    let (mut p_log_id, mut p_it, mut p_li) = (Any::Nil, Any::Nil, Any::Nil);
    let top = [
        ("SignedEntryTimestamp", "SignedEntryTimestamp"),
        ("Payload", "Payload"),
        ("logID", "logID"),
        ("integratedTime", "integratedTime"),
        ("logIndex", "logIndex"),
    ];
    let payload = [
        ("body", "body"),
        ("logID", "logID"),
        ("integratedTime", "integratedTime"),
        ("logIndex", "logIndex"),
    ];
    d.object(&v, "", &bundle_ty, &top, |d, i, x| match i {
        0 => d.bytes(x, &mut set),
        1 => d.object(x, "", &payload_ty, &payload, |d, j, y| match j {
            0 => d.bytes(y, &mut body),
            1 => d.any(y, &mut p_log_id),
            2 => d.any(y, &mut p_it),
            _ => d.any(y, &mut p_li),
        }),
        2 => d.any(x, &mut log_id),
        3 => d.any(x, &mut it),
        _ => d.any(x, &mut li),
    });
    d.done(()).map_err(|e| format!("parse bundle json: {e}"))?;
    let text = |a: &Any| match a {
        Any::Str(s) => s.clone(),
        _ => String::new(),
    };
    let mut log = text(&log_id);
    if log.is_empty() {
        log = text(&p_log_id);
    }
    let mut integrated = it.to_int64().unwrap_or(0);
    if integrated == 0
        && let Some(v) = p_it.to_int64()
    {
        integrated = v;
    }
    let mut index = li.to_int64().unwrap_or(0);
    if index == 0
        && let Some(v) = p_li.to_int64()
    {
        index = v;
    }
    Ok((body.into_vec(), set.into_vec(), log, integrated, index))
}

/// newHashedRecordSignedEntity.
fn hashed_record(m: &Manifest, dhi: bool) -> Result<Entity, String> {
    let desc = m.layers.first().ok_or("no layers in manifest")?;
    let sig_text = desc
        .annotations
        .get("dev.cosignproject.cosign/signature")
        .ok_or("no signature annotation found")?;
    let sig = crate::gobase64::decode(sig_text.as_bytes(), false, true)
        .map_err(|o| format!("decode signature: {}", crate::gobase64::error_text(o)))?;
    let (alg, hex) = desc.digest.split_once(':').unwrap_or(("", &desc.digest));
    let digest = crate::signature::hex_decode(hex)
        .ok_or_else(|| format!("decode digest: {}", crate::signature::hex_error(hex)))?;
    let certificate = if dhi {
        None
    } else {
        let pem = desc
            .annotations
            .get("dev.sigstore.cosign/certificate")
            .cloned()
            .unwrap_or_default();
        if pem.is_empty() {
            return Err("no certificate annotation found".into());
        }
        let (block, _) =
            crate::tlog::pem::decode(pem.as_bytes()).ok_or("no PEM certificate found in annotation")?;
        Some(crate::x509::Certificate::parse(&block.bytes).map_err(|e| e.0)?)
    };
    let entries = match desc
        .annotations
        .get("dev.sigstore.cosign/bundle")
        .filter(|b| !b.is_empty())
    {
        None => Ok(Vec::new()),
        Some(b) => (|| {
            let (body, set, log_id, it, li) =
                rekor_bundle(b.as_bytes()).map_err(|e| format!("parse rekor bundle: {e}"))?;
            let id = crate::signature::hex_decode(&log_id)
                .ok_or_else(|| format!("decode logID: {}", crate::signature::hex_error(&log_id)))?;
            let tle = crate::bundle::TlogEntry {
                log_index: li,
                log_id: Some((!id.is_empty()).then_some(id)),
                kind_version: Some(("hashedrekord".into(), "0.0.1".into())),
                integrated_time: it,
                inclusion_promise: Some((!set.is_empty()).then_some(set)),
                inclusion_proof: None,
                canonicalized_body: (!body.is_empty()).then_some(body),
            };
            crate::tlog::new_entry(&tle)
                .map(|e| vec![e])
                .map_err(|e| format!("create tlog entry: {e}"))
        })(),
    };
    Ok(Entity::hashed_record(
        digest,
        alg.to_string(),
        sig,
        certificate,
        entries,
    ))
}

/// VerifyImage: the signature of the image `desc` names (an index) for `platform`,
/// Docker Hardened Images' by their carried key.
pub fn verify_image<'r>(
    p: &dyn Provider,
    desc: &Descriptor,
    platform: &Platform,
    root: &Root<'r>,
    zone: Zone,
) -> Result<SignatureInfo, Error> {
    verify_image_with(p, desc, platform, root, zone, &dhi_key)
}

/// VerifyImage with the DHI key `dhi` gives.
pub fn verify_image_with<'r>(
    p: &dyn Provider,
    desc: &Descriptor,
    platform: &Platform,
    root: &Root<'r>,
    zone: Zone,
    dhi: &dyn Fn() -> Result<KeyMaterial, Error>,
) -> Result<SignatureInfo, Error> {
    let fail = |e: String| Error(e);
    let sc = resolve_signature_chain(p, desc, platform).map_err(|e| {
        fail(format!(
            "resolving signature chain for image {}: {e}",
            desc.digest
        ))
    })?;
    let (Some(att), Some(sig)) = (&sc.attestation_manifest, &sc.signature_manifest) else {
        return Err(fail(no_chain(&desc.digest, sc.attestation_manifest.is_some())));
    };
    let att_bytes =
        read_blob(p, att).map_err(|e| fail(format!("reading attestation manifest {}: {e}", att.digest)))?;
    let attestation = parse_manifest(&att_bytes)
        .map_err(|e| fail(format!("unmarshaling attestation manifest {}: {e}", att.digest)))?;
    let image = &sc.image_manifest;
    let Some(subject) = &attestation.subject else {
        return Err(fail(format!(
            "attestation manifest {} has no subject",
            att.digest
        )));
    };
    if subject.digest != image.digest {
        return Err(fail(format!(
            "attestation manifest {} subject digest {} does not match image manifest digest {}",
            att.digest, subject.digest, image.digest
        )));
    }
    if subject.media_type != MEDIA_MANIFEST && subject.media_type != MEDIA_INDEX {
        return Err(fail(format!(
            "attestation manifest {} subject media type {} is not an image manifest or index",
            att.digest, subject.media_type
        )));
    }
    if subject.size != image.size {
        return Err(fail(format!(
            "attestation manifest {} subject size {} does not match image manifest size {}",
            att.digest, subject.size, image.size
        )));
    }
    if !attestation.layers.iter().any(|l| {
        matches!(
            l.annotations.get(PREDICATE_TYPE).map(String::as_str),
            Some(SLSA_V02 | SLSA_V1)
        )
    }) {
        return Err(fail(format!(
            "attestation manifest {} has no SLSA provenance layer",
            att.digest
        )));
    }
    let root = root().map_err(fail)?;
    let sig_bytes =
        read_blob(p, sig).map_err(|e| fail(format!("reading signature manifest {}: {e}", sig.digest)))?;
    let m = parse_manifest(&sig_bytes)
        .map_err(|e| fail(format!("unmarshaling signature manifest {}: {e}", sig.digest)))?;
    let Some(subject) = &m.subject else {
        return Err(fail(format!("signature manifest {} has no subject", sig.digest)));
    };
    if subject.digest != att.digest {
        return Err(fail(format!(
            "signature manifest {} subject digest {} does not match attestation manifest digest {}",
            sig.digest, subject.digest, att.digest
        )));
    }
    if subject.media_type != MEDIA_MANIFEST && subject.media_type != MEDIA_INDEX {
        return Err(fail(format!(
            "signature manifest {} subject media type {} is not an image manifest or index",
            sig.digest, subject.media_type
        )));
    }
    if subject.size != att.size {
        return Err(fail(format!(
            "signature manifest {} subject size {} does not match attestation manifest size {}",
            sig.digest, subject.size, att.size
        )));
    }
    let Some(layer) = m.layers.first() else {
        return Err(fail(format!(
            "signature manifest {} has {} layers, expected 1",
            sig.digest,
            m.layers.len()
        )));
    };
    let mut docker_reference = String::new();
    let (entity, signature_type, digest) = match layer.media_type.as_str() {
        ARTIFACT_SIGSTORE_BUNDLE => {
            if m.artifact_type != ARTIFACT_SIGSTORE_BUNDLE {
                return Err(fail(format!(
                    "signature manifest {} is not a bundle (artifact type {})",
                    sig.digest,
                    shards_dockerfile::go::quote(m.artifact_type.as_bytes())
                )));
            }
            let b = read_blob(p, layer).map_err(|e| {
                fail(format!(
                    "reading bundle layer {} from signature manifest {}: {e}",
                    layer.digest, sig.digest
                ))
            })?;
            let bundle = crate::bundle::parse(&b).map_err(|e| {
                fail(format!(
                    "loading signature bundle from manifest {}: {}",
                    sig.digest, e.0
                ))
            })?;
            (
                Entity::from_bundle(&bundle),
                SignatureType::BundleV03,
                raw_digest(&att.digest).map_err(fail)?,
            )
        }
        MEDIA_SIMPLE_SIGNING => {
            let b = read_blob(p, layer).map_err(|e| {
                fail(format!(
                    "reading bundle layer {} from signature manifest {}: {e}",
                    layer.digest, sig.digest
                ))
            })?;
            let (reference, manifest_digest, ty) = simple_signing(&b).map_err(|e| {
                fail(format!(
                    "unmarshaling simple signing payload from manifest {}: {e}",
                    sig.digest
                ))
            })?;
            if manifest_digest != att.digest {
                return Err(fail(format!(
                    "simple signing payload in manifest {} has docker-manifest-digest {manifest_digest} which does not match attestation manifest digest {}",
                    sig.digest, att.digest
                )));
            }
            if ty != "cosign container image signature" {
                return Err(fail(format!(
                    "simple signing payload in manifest {} has invalid type {}",
                    sig.digest,
                    shards_dockerfile::go::quote(ty.as_bytes())
                )));
            }
            docker_reference = reference;
            let e = hashed_record(&m, sc.dhi).map_err(|e| {
                fail(format!(
                    "loading hashed record signed entity from manifest {}: {e}",
                    sig.digest
                ))
            })?;
            (
                e,
                SignatureType::SimpleSigningV1,
                raw_digest(&layer.digest).map_err(fail)?,
            )
        }
        other => {
            return Err(fail(format!(
                "signature manifest {} layer has invalid media type {other}",
                sig.digest
            )));
        }
    };
    let dhi_key = if sc.dhi {
        Some(dhi().map_err(|e| fail(format!("getting DHI trust root: {}", e.0)))?)
    } else {
        None
    };
    let (config, identity) = if sc.dhi {
        let config = if layer.annotations.contains_key("dev.sigstore.cosign/bundle") {
            Config {
                tlog: 1,
                observer: 1,
                ..Config::default()
            }
        } else {
            Config {
                no_observer: true,
                ..Config::default()
            }
        };
        (config, Identity::Unsafe)
    } else {
        (crate::helpers::ARTIFACT_CONFIG, Identity::Any)
    };
    let material = Material {
        root,
        key: dhi_key.as_ref(),
        fulcio: !sc.dhi,
    };
    let policy = Policy {
        digest: Some(digest),
        identity,
    };
    let outcome = verify::verify_entity(&entity, &material, &config, &policy, zone)
        .map_err(|e| fail(format!("verifying bundle: {}", e.0)))?;
    if outcome.certificate.is_none() && !sc.dhi {
        return Err(fail("no valid signatures found".into()));
    }
    let mut si = SignatureInfo {
        kind: Kind::Untrusted,
        signature_type,
        signer: outcome.certificate,
        timestamps: outcome.timestamps,
        docker_reference,
        is_dhi: sc.dhi,
    };
    si.kind = si.detect_kind();
    Ok(si)
}
