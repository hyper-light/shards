//! The documents an image is made of, as registries serve them: OCI image-spec v1.1.1
//! descriptors, indexes, manifests and configs, and Docker's schema 2 equivalents
//! (docs/research/registry-pull.md §4, §5).

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::reference::{Algorithm, Digest};
use crate::{Error, bad};

/// Media types (image-spec media-types.md; Docker's schema 2).
pub mod media {
    pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
    pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
    /// Schema 1, which Docker itself no longer pulls.
    pub const DOCKER_SCHEMA1: &str = "application/vnd.docker.distribution.manifest.v1+json";
    pub const DOCKER_SCHEMA1_SIGNED: &str = "application/vnd.docker.distribution.manifest.v1+prettyjws";
    pub const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
    pub const OCI_LAYER_NONDISTRIBUTABLE: &str = "application/vnd.oci.image.layer.nondistributable.v1.tar";
    pub const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar";
    pub const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
    pub const DOCKER_LAYER_ZSTD: &str = "application/vnd.docker.image.rootfs.diff.tar.zstd";
    pub const DOCKER_LAYER_FOREIGN: &str = "application/vnd.docker.image.rootfs.foreign.diff.tar";
    pub const DOCKER_LAYER_FOREIGN_GZIP: &str = "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip";
}

/// The largest manifest or index read: containerd's `MaxManifestSize`.
pub const MAX_MANIFEST: u64 = 4_393_216;

/// A reference to content: its type, digest and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    #[serde(default)]
    pub media_type: String,
    pub digest: String,
    pub size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

impl Descriptor {
    /// The digest, validated.
    pub fn digest(&self) -> Result<Digest, Error> {
        Digest::parse(&self.digest)
    }

    /// The size, which must not be negative.
    pub fn size(&self) -> Result<u64, Error> {
        u64::try_from(self.size).map_err(|_| Error(format!("descriptor size {} is negative", self.size)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Platform {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(rename = "os.features", default, skip_serializing_if = "Vec::is_empty")]
    pub os_features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub schema_version: u32,
    #[serde(default)]
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(default)]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

/// An image config: the platform, the runtime defaults, and the layers' DiffIDs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub config: Option<RunConfig>,
    pub rootfs: RootFs,
}

/// The config's defaults for a container, in Docker's field names.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RunConfig {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RootFs {
    #[serde(rename = "type")]
    pub kind: String,
    pub diff_ids: Vec<String>,
}

/// What a manifest document turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    Index(Index),
    Manifest(Manifest),
}

/// Parses an index or manifest, after checking its size, and that the registry's
/// `Content-Type` agrees with the document's own `mediaType` and shape (containerd's
/// `validateMediaType`). Schema 1 is refused, as Docker refuses it.
pub fn parse_document(bytes: &[u8], content_type: &str) -> Result<Document, Error> {
    if bytes.len() as u64 > MAX_MANIFEST {
        return bad(format!(
            "a {}-byte manifest is over the {MAX_MANIFEST}-byte limit",
            bytes.len()
        ));
    }
    let content_type = content_type.split(';').next().unwrap_or_default().trim();
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Shape {
        #[serde(default)]
        media_type: Option<String>,
        #[serde(default)]
        manifests: Option<serde_json::Value>,
        #[serde(default)]
        config: Option<serde_json::Value>,
        #[serde(default)]
        schema_version: Option<u32>,
    }
    let shape: Shape = serde_json::from_slice(bytes).map_err(|e| Error(format!("manifest: {e}")))?;
    let declared = shape.media_type.as_deref().unwrap_or(content_type);
    if !shape.media_type.as_deref().is_none_or(|m| m == content_type) && !content_type.is_empty() {
        return bad(format!(
            "the registry says {content_type}, but the document says {declared}"
        ));
    }
    if matches!(declared, media::DOCKER_SCHEMA1 | media::DOCKER_SCHEMA1_SIGNED)
        || shape.schema_version == Some(1)
    {
        return bad("schema 1 manifests are not supported, as Docker no longer supports them");
    }
    let is_index = shape.manifests.is_some();
    let is_manifest = shape.config.is_some();
    match declared {
        media::OCI_INDEX | media::DOCKER_LIST if is_index && !is_manifest => {}
        media::OCI_MANIFEST | media::DOCKER_MANIFEST if is_manifest && !is_index => {}
        other => {
            return bad(format!(
                "{other:?} is not an index or manifest this document can be"
            ));
        }
    }
    let parsed = if is_index {
        serde_json::from_slice(bytes).map(Document::Index)
    } else {
        serde_json::from_slice(bytes).map(Document::Manifest)
    };
    parsed.map_err(|e| Error(format!("manifest: {e}")))
}

/// Parses an image config, which must describe layers (config.md).
pub fn parse_config(bytes: &[u8]) -> Result<ImageConfig, Error> {
    let config: ImageConfig =
        serde_json::from_slice(bytes).map_err(|e| Error(format!("image config: {e}")))?;
    if config.rootfs.kind != "layers" {
        return bad(format!("rootfs type {:?} is not \"layers\"", config.rootfs.kind));
    }
    Ok(config)
}

/// How a layer's blob is read to get its tar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCompression {
    /// As it is.
    None,
    /// As gzip, zstd or plain tar, whichever its first bytes say.
    Sniffed,
}

/// How a layer of `media_type` is read, as containerd v2.4.1 decides (`DiffCompression`
/// in `core/images/mediatypes.go`, `compressedHandler` in `core/diff/stream.go`):
/// - Docker's layer types are sniffed, since writers mislabel them. With a `+` suffix
///   they are wrapped, and read as they are.
/// - OCI's layer types are sniffed when their last suffix, sorted, is `gzip` or `zstd`,
///   and read as they are otherwise.
/// - Anything else is not a layer. Encrypted layers are refused too: containerd needs
///   ocicrypt for them, and we cannot decrypt them.
pub fn layer_compression(media_type: &str) -> Result<LayerCompression, Error> {
    let (base, ext) = match media_type.split_once('+') {
        Some((base, ext)) => (base, Some(ext)),
        None => (media_type, None),
    };
    // containerd's parseMediaTypes: at most 50 suffixes, sorted.
    let suffixes: Vec<&str> = ext.map(|e| e.splitn(50, '+').collect()).unwrap_or_default();
    if suffixes.contains(&"encrypted") {
        return bad(format!("{media_type}: encrypted layers are not supported"));
    }
    match base {
        media::DOCKER_LAYER
        | media::DOCKER_LAYER_GZIP
        | media::DOCKER_LAYER_ZSTD
        | media::DOCKER_LAYER_FOREIGN
        | media::DOCKER_LAYER_FOREIGN_GZIP => Ok(if suffixes.is_empty() {
            LayerCompression::Sniffed
        } else {
            LayerCompression::None
        }),
        media::OCI_LAYER | media::OCI_LAYER_NONDISTRIBUTABLE => Ok(match suffixes.iter().max() {
            Some(&("gzip" | "zstd")) => LayerCompression::Sniffed,
            _ => LayerCompression::None,
        }),
        _ => bad(format!("{media_type}: not a layer")),
    }
}

/// A layer stack's ChainID (image-spec config.md): the first DiffID, then the SHA-256 of
/// the previous ChainID and the next DiffID, as text, joined by a space.
pub fn chain_id(diff_ids: &[Digest]) -> Option<Digest> {
    let mut ids = diff_ids.iter();
    let mut chain = ids.next()?.clone();
    for id in ids {
        let hash = Sha256::digest(format!("{chain} {id}").as_bytes());
        chain = Digest::from_hash(Algorithm::Sha256, &hash);
    }
    Some(chain)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const A: &str = "sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b";
    const B: &str = "sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef";

    fn index() -> String {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[
                {{"mediaType":"{}","digest":"{A}","size":7143,"platform":{{"architecture":"arm64","os":"linux","variant":"v8"}}}},
                {{"mediaType":"{}","digest":"{B}","size":7682,"platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#,
            media::OCI_INDEX,
            media::OCI_MANIFEST,
            media::OCI_MANIFEST
        )
    }

    fn manifest() -> String {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{A}","size":7023}},
               "layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{B}","size":32654}}]}}"#,
            media::OCI_MANIFEST
        )
    }

    #[test]
    fn documents_parse_when_their_types_agree() {
        let Document::Index(i) = parse_document(index().as_bytes(), media::OCI_INDEX).unwrap() else {
            panic!("not an index");
        };
        assert_eq!(i.manifests.len(), 2);
        assert_eq!(
            i.manifests[0].platform.as_ref().unwrap().variant.as_deref(),
            Some("v8")
        );
        let Document::Manifest(m) = parse_document(manifest().as_bytes(), media::OCI_MANIFEST).unwrap()
        else {
            panic!("not a manifest");
        };
        assert_eq!((m.layers.len(), m.config.size().unwrap()), (1, 7023));
        // A registry that sends no Content-Type is taken at the document's word.
        assert!(parse_document(manifest().as_bytes(), "").is_ok());
    }

    #[test]
    fn mismatched_and_schema1_documents_are_refused() {
        assert!(
            parse_document(index().as_bytes(), media::OCI_MANIFEST).is_err(),
            "an index sent as a manifest"
        );
        let posing = manifest().replace(media::OCI_MANIFEST, media::OCI_INDEX);
        assert!(
            parse_document(posing.as_bytes(), media::OCI_INDEX).is_err(),
            "a manifest calling itself an index"
        );
        let schema1 = r#"{"schemaVersion":1,"name":"x","tag":"y","fsLayers":[]}"#;
        assert!(parse_document(schema1.as_bytes(), media::DOCKER_SCHEMA1_SIGNED).is_err());
        let huge = vec![b' '; MAX_MANIFEST as usize + 1];
        assert!(parse_document(&huge, media::OCI_MANIFEST).is_err());
    }

    #[test]
    fn configs_carry_their_defaults_and_diff_ids() {
        let json = r#"{"architecture":"arm64","os":"linux","config":{"User":"app","Env":["PATH=/bin"],"Entrypoint":["/bin/sh"],"Cmd":["-c","true"],"WorkingDir":"/srv"},
            "rootfs":{"type":"layers","diff_ids":["sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b"]}}"#;
        let c = parse_config(json.as_bytes()).unwrap();
        let run = c.config.unwrap();
        assert_eq!(run.user.as_deref(), Some("app"));
        assert_eq!(run.cmd, Some(vec!["-c".to_string(), "true".to_string()]));
        assert_eq!(run.working_dir.as_deref(), Some("/srv"));
        assert!(parse_config(json.replace("\"layers\"", "\"other\"").as_bytes()).is_err());
    }

    #[test]
    fn layers_are_read_as_containerd_reads_their_media_types() {
        use LayerCompression::{None as Raw, Sniffed};
        for (media_type, want) in [
            ("application/vnd.oci.image.layer.v1.tar", Some(Raw)),
            ("application/vnd.oci.image.layer.v1.tar+gzip", Some(Sniffed)),
            ("application/vnd.oci.image.layer.v1.tar+zstd", Some(Sniffed)),
            ("application/vnd.oci.image.layer.v1.tar+other", Some(Raw)),
            ("application/vnd.oci.image.layer.v1.tar+gzip+zz", Some(Raw)),
            (
                "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
                Some(Sniffed),
            ),
            ("application/vnd.docker.image.rootfs.diff.tar", Some(Sniffed)),
            ("application/vnd.docker.image.rootfs.diff.tar.gzip", Some(Sniffed)),
            ("application/vnd.docker.image.rootfs.diff.tar.zstd", Some(Sniffed)),
            (
                "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
                Some(Sniffed),
            ),
            (
                "application/vnd.docker.image.rootfs.diff.tar.gzip+wrapped",
                Some(Raw),
            ),
            ("application/vnd.oci.image.layer.v1.tar+gzip+encrypted", None),
            ("application/vnd.oci.image.config.v1+json", None),
            ("", None),
        ] {
            assert_eq!(layer_compression(media_type).ok(), want, "{media_type}");
        }
    }

    /// Known answers, computed with `printf '%s %s' ... | shasum -a 256`.
    #[test]
    fn chain_ids_hash_the_digest_text() {
        const C: &str = "sha256:13cb14c2acd34e45446a50af25cb05095a17624678dbafbcc9e26086547c1d74";
        let ids: Vec<Digest> = [A, B, C].iter().map(|d| Digest::parse(d).unwrap()).collect();
        let chain = |n: usize| chain_id(&ids[..n]).map(|d| d.to_string());
        assert_eq!(chain(0), None);
        assert_eq!(chain(1).as_deref(), Some(A));
        assert_eq!(
            chain(2).as_deref(),
            Some("sha256:dd6bc33f5aa05ef0be66a7c578843474c2c158c9ee3a27a63313d296dcf2ce4c")
        );
        assert_eq!(
            chain(3).as_deref(),
            Some("sha256:189ff9a011cbb6051b05a0c07a9ff6821f91182a13079f2f65400b98c741254d")
        );
    }
}
