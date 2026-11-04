//! What a policy command run through a context (D180) asks of the machine that is its
//! builder, and what the machine answers: a source and what its policy asks to know of it
//! (ResolveSourceMetaRequest), and what is known of it (ResolveSourceMetaResponse), as
//! JSON, each byte string as base64, each field whole, so that the client evaluates the
//! answer as the machine's builds would.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use shards_agentfile::platform::Platform;

use super::{
    ArtifactMeta, AttestationChain, ChecksumRequest, GitMeta, GitRequest, HttpMeta, ImageMeta, ImageRequest,
    Meta, MetaRequest, Source,
};

/// A source and what is asked of it.
#[derive(Serialize, Deserialize)]
struct Ask {
    identifier: String,
    attrs: BTreeMap<String, String>,
    platform: Option<Plat>,
    image: Option<Image>,
    git: Option<bool>,
    http: bool,
    http_checksum: Option<(String, B64)>,
}

#[derive(Serialize, Deserialize)]
struct Image {
    no_config: bool,
    attestation_chain: bool,
    resolve_attestations: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Plat {
    os: B64,
    architecture: B64,
    variant: B64,
    os_version: B64,
    os_features: Vec<B64>,
}

/// What is known.
#[derive(Serialize, Deserialize)]
struct Known {
    image: Option<KnownImage>,
    git: Option<KnownGit>,
    http: Option<KnownHttp>,
    artifact: Option<KnownArtifact>,
}

#[derive(Serialize, Deserialize)]
struct KnownImage {
    digest: String,
    config: Option<B64>,
    attestation_chain: Option<Chain>,
}

#[derive(Serialize, Deserialize)]
struct Chain {
    root: String,
    attestation_manifest: String,
    signature_manifests: Vec<String>,
    blobs: BTreeMap<String, (Descriptor, B64)>,
}

#[derive(Serialize, Deserialize)]
struct Descriptor {
    media_type: String,
    digest: String,
    size: i64,
    urls: Vec<String>,
    annotations: BTreeMap<String, String>,
    data: B64,
    platform: Option<DescriptorPlatform>,
    artifact_type: String,
}

#[derive(Serialize, Deserialize)]
struct DescriptorPlatform {
    architecture: String,
    os: String,
    os_version: String,
    os_features: Vec<String>,
    variant: String,
}

#[derive(Serialize, Deserialize)]
struct KnownGit {
    reference: String,
    checksum: String,
    commit_checksum: String,
    commit_object: Option<B64>,
    tag_object: Option<B64>,
}

#[derive(Serialize, Deserialize)]
struct KnownHttp {
    checksum: String,
    signature_checksum: Option<(String, B64)>,
}

#[derive(Serialize, Deserialize)]
struct KnownArtifact {
    digest: String,
    artifact_type: String,
    signatures: Option<Vec<B64>>,
}

/// Bytes, as base64 in the JSON.
#[derive(Default)]
struct B64(Vec<u8>);

impl Serialize for B64 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for B64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<B64, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map(B64)
            .map_err(serde::de::Error::custom)
    }
}

fn b64(bytes: &[u8]) -> B64 {
    B64(bytes.to_vec())
}

fn plat(p: &Platform) -> Plat {
    Plat {
        os: b64(&p.os),
        architecture: b64(&p.architecture),
        variant: b64(&p.variant),
        os_version: b64(&p.os_version),
        os_features: p.os_features.iter().map(|f| b64(f)).collect(),
    }
}

fn platform_of(p: Plat) -> Platform {
    Platform {
        os: p.os.0,
        architecture: p.architecture.0,
        variant: p.variant.0,
        os_version: p.os_version.0,
        os_features: p.os_features.into_iter().map(|f| f.0).collect(),
    }
}

/// The checksum algorithms a request may name, as [`ChecksumRequest`] holds them.
fn algorithm(name: &str) -> Result<&'static str, String> {
    match name {
        "sha256" => Ok("sha256"),
        "sha384" => Ok("sha384"),
        "sha512" => Ok("sha512"),
        other => Err(format!("{other:?}: no checksum algorithm")),
    }
}

/// A platform, as the machine says its own.
pub fn platform(p: &Platform) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&plat(p)).map_err(|e| e.to_string())
}

pub fn read_platform(bytes: &[u8]) -> Result<Platform, String> {
    serde_json::from_slice(bytes)
        .map(platform_of)
        .map_err(|e| format!("the machine's platform: {e}"))
}

/// `source` and `request`, as the client asks them.
pub fn ask(source: &Source, request: &MetaRequest) -> Result<Vec<u8>, String> {
    let asked = Ask {
        identifier: source.identifier.clone(),
        attrs: source.attrs.clone(),
        platform: request.platform.as_ref().map(plat),
        image: request.image.as_ref().map(|i| Image {
            no_config: i.no_config,
            attestation_chain: i.attestation_chain,
            resolve_attestations: i.resolve_attestations.clone(),
        }),
        git: request.git.as_ref().map(|g| g.return_object),
        http: request.http,
        http_checksum: request
            .http_checksum
            .as_ref()
            .map(|c| (c.algorithm.to_string(), b64(&c.suffix))),
    };
    serde_json::to_vec(&asked).map_err(|e| e.to_string())
}

/// What [`ask`] wrote, as the machine reads it.
pub fn read_ask(bytes: &[u8]) -> Result<(Source, MetaRequest), String> {
    let asked: Ask = serde_json::from_slice(bytes).map_err(|e| format!("a policy's request: {e}"))?;
    let http_checksum = match asked.http_checksum {
        Some((name, suffix)) => Some(ChecksumRequest {
            algorithm: algorithm(&name)?,
            suffix: suffix.0,
        }),
        None => None,
    };
    Ok((
        Source {
            identifier: asked.identifier,
            attrs: asked.attrs,
        },
        MetaRequest {
            platform: asked.platform.map(platform_of),
            image: asked.image.map(|i| ImageRequest {
                no_config: i.no_config,
                attestation_chain: i.attestation_chain,
                resolve_attestations: i.resolve_attestations,
            }),
            git: asked.git.map(|return_object| GitRequest { return_object }),
            http: asked.http,
            http_checksum,
        },
    ))
}

/// What the machine knows of a source, as it answers.
pub fn known(meta: &Meta) -> Result<Vec<u8>, String> {
    let known = Known {
        image: meta.image.as_ref().map(|i| KnownImage {
            digest: i.digest.clone(),
            config: i.config.as_deref().map(b64),
            attestation_chain: i.attestation_chain.as_ref().map(|c| Chain {
                root: c.root.clone(),
                attestation_manifest: c.attestation_manifest.clone(),
                signature_manifests: c.signature_manifests.clone(),
                blobs: c
                    .blobs
                    .iter()
                    .map(|(k, (d, bytes))| (k.clone(), (descriptor(d), b64(bytes))))
                    .collect(),
            }),
        }),
        git: meta.git.as_ref().map(|g| KnownGit {
            reference: g.reference.clone(),
            checksum: g.checksum.clone(),
            commit_checksum: g.commit_checksum.clone(),
            commit_object: g.commit_object.as_deref().map(b64),
            tag_object: g.tag_object.as_deref().map(b64),
        }),
        http: meta.http.as_ref().map(|h| KnownHttp {
            checksum: h.checksum.clone(),
            signature_checksum: h.signature_checksum.as_ref().map(|(d, s)| (d.clone(), b64(s))),
        }),
        artifact: meta.artifact.as_ref().map(|a| KnownArtifact {
            digest: a.digest.clone(),
            artifact_type: a.artifact_type.clone(),
            signatures: a.signatures.as_ref().map(|s| s.iter().map(|b| b64(b)).collect()),
        }),
    };
    serde_json::to_vec(&known).map_err(|e| e.to_string())
}

/// What [`known`] wrote, as the client reads it.
pub fn read_known(bytes: &[u8]) -> Result<Meta, String> {
    let known: Known = serde_json::from_slice(bytes).map_err(|e| format!("the machine's answer: {e}"))?;
    Ok(Meta {
        image: known.image.map(|i| ImageMeta {
            digest: i.digest,
            config: i.config.map(|c| c.0),
            attestation_chain: i.attestation_chain.map(|c| AttestationChain {
                root: c.root,
                attestation_manifest: c.attestation_manifest,
                signature_manifests: c.signature_manifests,
                blobs: c
                    .blobs
                    .into_iter()
                    .map(|(k, (d, bytes))| (k, (descriptor_of(d), bytes.0)))
                    .collect(),
            }),
        }),
        git: known.git.map(|g| GitMeta {
            reference: g.reference,
            checksum: g.checksum,
            commit_checksum: g.commit_checksum,
            commit_object: g.commit_object.map(|o| o.0),
            tag_object: g.tag_object.map(|o| o.0),
        }),
        http: known.http.map(|h| HttpMeta {
            checksum: h.checksum,
            signature_checksum: h.signature_checksum.map(|(d, s)| (d, s.0)),
        }),
        artifact: known.artifact.map(|a| ArtifactMeta {
            digest: a.digest,
            artifact_type: a.artifact_type,
            signatures: a.signatures.map(|s| s.into_iter().map(|b| b.0).collect()),
        }),
    })
}

fn descriptor(d: &shards_sigstore::image::Descriptor) -> Descriptor {
    Descriptor {
        media_type: d.media_type.clone(),
        digest: d.digest.clone(),
        size: d.size,
        urls: d.urls.clone(),
        annotations: d.annotations.clone(),
        data: b64(&d.data),
        platform: d.platform.as_ref().map(|p| DescriptorPlatform {
            architecture: p.architecture.clone(),
            os: p.os.clone(),
            os_version: p.os_version.clone(),
            os_features: p.os_features.clone(),
            variant: p.variant.clone(),
        }),
        artifact_type: d.artifact_type.clone(),
    }
}

fn descriptor_of(d: Descriptor) -> shards_sigstore::image::Descriptor {
    shards_sigstore::image::Descriptor {
        media_type: d.media_type,
        digest: d.digest,
        size: d.size,
        urls: d.urls,
        annotations: d.annotations,
        data: d.data.0,
        platform: d.platform.map(|p| shards_sigstore::platforms::Platform {
            architecture: p.architecture,
            os: p.os,
            os_version: p.os_version,
            os_features: p.os_features,
            variant: p.variant,
        }),
        artifact_type: d.artifact_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request and an answer cross whole: every field the client asked, every field the
    /// machine knew, the bytes among them as they were.
    #[test]
    fn a_request_and_its_answer_cross_whole() {
        let mut source = Source::new("docker-image://docker.io/library/alpine:3.22");
        source.attrs.insert("image.recordtype".into(), "internal".into());
        let mut platform = Platform::new("linux", "arm64");
        platform.variant = b"v8".to_vec();
        platform.os_features = vec![b"f1".to_vec()];
        let request = MetaRequest {
            platform: Some(platform),
            image: Some(ImageRequest {
                no_config: true,
                attestation_chain: true,
                resolve_attestations: vec!["https://slsa.dev/provenance/v1".into()],
            }),
            git: Some(GitRequest { return_object: true }),
            http: true,
            http_checksum: Some(ChecksumRequest {
                algorithm: "sha384",
                suffix: vec![0, 1, 255],
            }),
        };
        let (s, r) = read_ask(&ask(&source, &request).unwrap()).unwrap();
        assert_eq!((&s, &r), (&source, &request));

        let descriptor = shards_sigstore::image::Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: "sha256:aa".into(),
            size: 3,
            urls: vec!["https://u".into()],
            annotations: BTreeMap::from([("k".into(), "v".into())]),
            data: vec![7, 8],
            platform: Some(shards_sigstore::platforms::Platform {
                architecture: "arm64".into(),
                os: "linux".into(),
                os_version: String::new(),
                os_features: vec!["x".into()],
                variant: "v8".into(),
            }),
            artifact_type: "t".into(),
        };
        let meta = Meta {
            image: Some(ImageMeta {
                digest: "sha256:root".into(),
                config: Some(b"{\"os\":\"linux\"}".to_vec()),
                attestation_chain: Some(AttestationChain {
                    root: "sha256:root".into(),
                    attestation_manifest: "sha256:att".into(),
                    signature_manifests: vec!["sha256:sig".into()],
                    blobs: BTreeMap::from([("sha256:aa".into(), (descriptor, vec![0, 255, 10]))]),
                }),
            }),
            git: Some(GitMeta {
                reference: "refs/heads/main".into(),
                checksum: "c1".into(),
                commit_checksum: "c2".into(),
                commit_object: Some(b"tree x".to_vec()),
                tag_object: None,
            }),
            http: Some(HttpMeta {
                checksum: "sha256:h".into(),
                signature_checksum: Some(("sha384:s".into(), vec![1, 2])),
            }),
            artifact: Some(ArtifactMeta {
                digest: "sha256:art".into(),
                artifact_type: "application/vnd.shards.agent".into(),
                signatures: Some(vec![b"bundle".to_vec()]),
            }),
        };
        let back = read_known(&known(&meta).unwrap()).unwrap();
        assert_eq!(format!("{back:?}"), format!("{meta:?}"));
        assert_eq!(
            read_platform(&super::platform(&Platform::new("linux", "amd64")).unwrap()).unwrap(),
            Platform::new("linux", "amd64")
        );
        // A checksum algorithm the client may not name is refused, not taken as SHA-256.
        let mut odd: serde_json::Value = serde_json::from_slice(&ask(&source, &request).unwrap()).unwrap();
        odd["http_checksum"][0] = "md5".into();
        assert!(read_ask(&serde_json::to_vec(&odd).unwrap()).is_err());
    }
}
