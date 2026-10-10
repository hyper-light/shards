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
/// The largest image config read: containers/image v5.36.2's `MaxConfigBodySize`
/// (`internal/iolimits/iolimits.go`), which Podman, CRI-O and skopeo read configs under.
pub const MAX_CONFIG: u64 = 4 << 20;

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
    /// Kept as written, and written only if any: an index's tell which of its manifests
    /// are attestations (`vnd.docker.reference.type`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub annotations: std::collections::BTreeMap<String, String>,
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
    #[serde(default, deserialize_with = "list")]
    pub manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(default)]
    pub media_type: Option<String>,
    /// An artifact's type: a shards microVM's is `save::MICROVM`.
    #[serde(default)]
    pub artifact_type: Option<String>,
    pub config: Descriptor,
    /// `null` where an image has none, as BuildKit writes a layerless image's (measured,
    /// BuildKit v0.28.1: `FROM scratch` and `LABEL`).
    #[serde(default, deserialize_with = "list")]
    pub layers: Vec<Descriptor>,
}

impl Manifest {
    /// The image config: a shards microVM's is the one it carries (save::MICROVM_CONFIG),
    /// its own config empty, as an artifact's is.
    pub fn image_config(&self) -> &Descriptor {
        if self.artifact_type.as_deref() == Some(crate::save::MICROVM)
            && let Some(carried) = self
                .layers
                .iter()
                .find(|l| l.media_type == crate::save::MICROVM_CONFIG)
        {
            return carried;
        }
        &self.config
    }
}

/// An image config: the platform, the runtime defaults, and the layers' DiffIDs, as Docker
/// reads them ([`crate::config`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    pub variant: Option<String>,
    /// When it was made, as Docker writes it (`Time.Format(time.RFC3339Nano)`).
    pub created: Option<String>,
    pub config: Option<RunConfig>,
    pub rootfs: RootFs,
    /// What running, creating, tagging or inspecting the image fails with, where it does:
    /// Go's error reading the config into a `DockerOCIImage` (daemon/containerd
    /// `GetImage`), which Docker says after `could not deserialize image config: `.
    pub run_error: Option<String>,
}

/// The config's defaults for a container, as Docker's container config takes them from
/// the image's (daemon/containerd imagespec.go): `None` where Go's is nil or empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunConfig {
    pub user: Option<String>,
    pub env: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub stop_signal: Option<String>,
    pub healthcheck: Option<HealthConfig>,
    /// The shell `CMD-SHELL` health checks run in (the image's `SHELL`).
    pub shell: Option<Vec<String>>,
    /// Its `EXPOSE`d ports as a container's config keeps them: those `network.ParsePort`
    /// takes, `80/tcp` and the like, each once, in order.
    pub exposed_ports: Vec<String>,
    /// Its `VOLUME`s, in order.
    pub volumes: Vec<String>,
    pub labels: Option<std::collections::BTreeMap<String, String>>,
}

/// A list, or none for `null`, as Go reads a slice.
fn list<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    let v: Option<Vec<T>> = serde::Deserialize::deserialize(d)?;
    Ok(v.unwrap_or_default())
}

/// A `HEALTHCHECK` as an image config holds it: its test, then durations in nanoseconds,
/// each 0 for "not set" (moby api/types/container HealthConfig).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HealthConfig {
    pub test: Option<Vec<String>>,
    pub interval: i64,
    pub timeout: i64,
    pub start_period: i64,
    pub start_interval: i64,
    pub retries: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootFs {
    pub kind: String,
    /// Empty where Go's is nil (`null`, as BuildKit writes an image without layers).
    pub diff_ids: Vec<String>,
}

impl ImageConfig {
    /// The image config `image` is, `run_error` what Docker fails with running it.
    fn new(image: crate::config::Image, run_error: Option<String>) -> ImageConfig {
        let some = |s: String| (!s.is_empty()).then_some(s);
        let c = image.config;
        ImageConfig {
            architecture: image.architecture,
            os: image.os,
            variant: some(image.variant),
            created: image.created.map(|t| t.format_rfc3339_nano()),
            config: Some(RunConfig {
                user: some(c.user),
                env: c.env,
                entrypoint: c.entrypoint,
                cmd: c.cmd,
                working_dir: some(c.working_dir),
                stop_signal: some(c.stop_signal),
                healthcheck: c.healthcheck.map(|h| HealthConfig {
                    test: h.test,
                    interval: h.interval,
                    timeout: h.timeout,
                    start_period: h.start_period,
                    start_interval: h.start_interval,
                    retries: h.retries,
                }),
                shell: c.shell,
                exposed_ports: c
                    .exposed_ports
                    .iter()
                    .flatten()
                    .filter_map(|p| crate::config::parse_port(p))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                volumes: c.volumes.into_iter().flatten().collect(),
                labels: c.labels,
            }),
            rootfs: RootFs {
                kind: image.rootfs.kind,
                diff_ids: image.rootfs.diff_ids.unwrap_or_default(),
            },
            run_error,
        }
    }
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

/// Parses an image config, which must describe layers (config.md), as a pull or a load
/// takes it: one Go reads into no type is no config, but one only running fails on is
/// stored, as Docker 29.3.1 stores it (measured: `load` and `pull` of a config whose
/// `history` is a number, or whose `created` is no date, succeed, and `run`, `create`,
/// `tag` and `inspect` fail); what running it fails with is kept
/// ([`ImageConfig::run_error`]).
pub fn parse_config(bytes: &[u8]) -> Result<ImageConfig, Error> {
    let read = crate::config::Read::new(bytes).map_err(|e| Error(format!("image config: {e}")))?;
    let run_error = read.error(crate::config::As::Docker);
    let config = ImageConfig::new(read.into_image(), run_error);
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

    /// A layerless image as BuildKit v0.28.1 writes it (`FROM scratch`, `LABEL`; measured
    /// in Docker 29.3.1): `layers` and `diff_ids` null, read as none, as Go reads them.
    #[test]
    fn a_layerless_image_reads_as_go_reads_it() {
        let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:38f5d4340455270de28be24536c9342bdb1b430bdb62347e2ddb601edd3e7db0","size":321},"layers":null}"#;
        let Document::Manifest(m) = parse_document(manifest, media::OCI_MANIFEST).unwrap() else {
            panic!("not a manifest");
        };
        assert!(m.layers.is_empty());
        let config = br#"{"architecture":"arm64","config":{"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"WorkingDir":"/","Labels":{"tier":"web"}},"created":null,"history":[{"created_by":"LABEL tier=web","comment":"buildkit.dockerfile.v0","empty_layer":true}],"os":"linux","rootfs":{"type":"layers","diff_ids":null}}"#;
        let c = parse_config(config).unwrap();
        assert!(c.rootfs.diff_ids.is_empty());
    }

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

    /// A config read as Docker reads it (crate::config): its keys folded and repeated as
    /// Go takes them, its ports as a container keeps them, its time as Docker writes it;
    /// one only running fails on stored, with what running it says.
    #[test]
    fn configs_are_read_as_docker_reads_them() {
        let json = r#"{"architecture":"arm64","os":"linux","variant":"","created":"2024-01-02T03:04:05.100000000+24:00",
            "config":{"User":"root","user":"nobody","Env":["A","B"],"Env":[null],"ExposedPorts":{"80/TCP":{},"x":{},"53/udp":{},"80":{}}},
            "rootfs":{"type":"layers","diff_ids":[]}}"#;
        let c = parse_config(json.as_bytes()).unwrap();
        let run = c.config.as_ref().unwrap();
        assert_eq!(run.user.as_deref(), Some("nobody"));
        assert_eq!(run.env, Some(vec!["A".to_string()]));
        assert_eq!(
            run.exposed_ports,
            vec!["53/udp".to_string(), "80/tcp".to_string()]
        );
        assert_eq!(c.variant, None);
        assert_eq!(c.created.as_deref(), Some("2024-01-02T03:04:05.1+24:00"));
        assert_eq!(c.run_error, None);
        // Stored, as Docker's load and pull store it; refused to run with Go's words.
        for (json, said) in [
            (
                r#"{"config":{"Healthcheck":5},"rootfs":{"type":"layers","diff_ids":[]}}"#,
                "json: cannot unmarshal number into Go struct field DockerOCIImageConfig.config.DockerOCIImageConfigExt.Healthcheck of type v1.HealthcheckConfig",
            ),
            (
                r#"{"created":"2024-02-30T00:00:00Z","rootfs":{"type":"layers","diff_ids":[]}}"#,
                "parsing time \"2024-02-30T00:00:00Z\": day out of range",
            ),
        ] {
            let c = parse_config(json.as_bytes()).unwrap();
            assert_eq!(c.run_error.as_deref(), Some(said), "{json}");
        }
        // No JSON at all is no config.
        assert!(parse_config(b"{").is_err());
    }

    /// The reader `parse_config` used before Go's (serde, by exact key), kept here alone to
    /// measure against (M128): its types and its reading, as they were at aa7a2d3.
    mod serde_reader {
        use std::collections::BTreeMap;

        use serde::Deserialize;

        #[derive(Deserialize)]
        #[allow(dead_code)]
        pub struct ImageConfig {
            #[serde(default)]
            architecture: String,
            #[serde(default)]
            os: String,
            #[serde(default)]
            variant: Option<String>,
            #[serde(default)]
            created: Option<String>,
            #[serde(default)]
            config: Option<RunConfig>,
            rootfs: RootFs,
        }

        #[derive(Deserialize, Default)]
        #[serde(rename_all = "PascalCase")]
        #[allow(dead_code)]
        struct RunConfig {
            #[serde(default)]
            user: Option<String>,
            #[serde(default)]
            env: Option<Vec<String>>,
            #[serde(default)]
            entrypoint: Option<Vec<String>>,
            #[serde(default)]
            cmd: Option<Vec<String>>,
            #[serde(default)]
            working_dir: Option<String>,
            #[serde(default)]
            stop_signal: Option<String>,
            #[serde(default)]
            healthcheck: Option<HealthConfig>,
            #[serde(default)]
            shell: Option<Vec<String>>,
            #[serde(default, deserialize_with = "keys")]
            exposed_ports: Vec<String>,
            #[serde(default, deserialize_with = "keys")]
            volumes: Vec<String>,
            #[serde(default)]
            labels: Option<BTreeMap<String, String>>,
        }

        #[derive(Deserialize, Default)]
        #[serde(rename_all = "PascalCase")]
        #[allow(dead_code)]
        struct HealthConfig {
            #[serde(default)]
            test: Option<Vec<String>>,
            #[serde(default)]
            interval: i64,
            #[serde(default)]
            timeout: i64,
            #[serde(default)]
            start_period: i64,
            #[serde(default)]
            start_interval: i64,
            #[serde(default)]
            retries: i64,
        }

        #[derive(Deserialize)]
        struct RootFs {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default, deserialize_with = "list")]
            #[allow(dead_code)]
            diff_ids: Vec<String>,
        }

        fn list<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
            let v: Option<Vec<T>> = Deserialize::deserialize(d)?;
            Ok(v.unwrap_or_default())
        }

        fn keys<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
            let map: Option<BTreeMap<String, serde::de::IgnoredAny>> = Deserialize::deserialize(d)?;
            Ok(map.map(|m| m.into_keys().collect()).unwrap_or_default())
        }

        pub fn parse_config(bytes: &[u8]) -> Result<ImageConfig, String> {
            let config: ImageConfig =
                serde_json::from_slice(bytes).map_err(|e| format!("image config: {e}"))?;
            if config.rootfs.kind != "layers" {
                return Err(format!("rootfs type {:?} is not \"layers\"", config.rootfs.kind));
            }
            Ok(config)
        }
    }

    /// M128: what reading an image's config costs (`parse_config`, which every pull, load,
    /// listing and run takes), for a config of a BuildKit build's size, one with a long
    /// history, and one near the largest read (`MAX_CONFIG`): n readings each by Go's
    /// reader and by the serde reader it replaced, interleaved, each reading's microseconds
    /// at p50, p90, p99 and max. docs/research/measurements/image-config/run.sh runs it.
    /// Where a build-sized config's reading goes (M128): Go's scanner building the
    /// document, then its fields read into each Go type, then `ImageConfig`, against the
    /// serde reader before it; nanoseconds a reading, the mean of n after a warm-up.
    #[test]
    #[ignore]
    fn parse_config_phases() {
        let history = (0..12)
            .map(|i| {
                format!(
                    r#"{{"created":"2024-01-02T03:04:05.{i:09}Z","created_by":"RUN /bin/sh -c step {i} && make install","comment":"buildkit.dockerfile.v0"{}}}"#,
                    if i % 3 == 0 { r#","empty_layer":true"# } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let env: Vec<String> = (0..8).map(|i| format!(r#""VAR_{i}=value-{i}""#)).collect();
        let labels: Vec<String> = (0..8)
            .map(|i| format!(r#""org.example.label.{i}":"{i}""#))
            .collect();
        let diff_ids: Vec<String> = (0..4).map(|i| format!(r#""sha256:{i:064x}""#)).collect();
        let text = format!(
            r#"{{"architecture":"arm64","os":"linux","created":"2024-01-02T03:04:05Z","config":{{"User":"app","Env":[{}],"Entrypoint":["/bin/app"],"Cmd":["serve"],"WorkingDir":"/srv","Labels":{{{}}},"ExposedPorts":{{"80/tcp":{{}},"443/tcp":{{}}}},"Volumes":{{"/data":{{}}}},"StopSignal":"SIGTERM"}},"rootfs":{{"type":"layers","diff_ids":[{}]}},"history":[{history}]}}"#,
            env.join(","),
            labels.join(","),
            diff_ids.join(",")
        );
        let b = text.as_bytes();
        let n = 20_000u32;
        let mean = |f: &dyn Fn()| {
            for _ in 0..1000 {
                f();
            }
            let at = std::time::Instant::now();
            for _ in 0..n {
                f();
            }
            at.elapsed().as_nanos() / u128::from(n)
        };
        let scan = mean(&|| {
            std::hint::black_box(crate::json::parse(std::hint::black_box(b)).unwrap());
        });
        let read = mean(&|| {
            std::hint::black_box(crate::config::Read::new(std::hint::black_box(b)).unwrap());
        });
        let whole = mean(&|| {
            std::hint::black_box(parse_config(std::hint::black_box(b)).unwrap());
        });
        let serde = mean(&|| {
            std::hint::black_box(serde_reader::parse_config(std::hint::black_box(b)).unwrap());
        });
        println!(
            "phases: {} bytes, n={n}, ns: scan {scan}, scan+fields {read}, whole {whole}, serde {serde}",
            b.len()
        );
    }

    #[test]
    #[ignore]
    fn parse_config_costs() {
        let history = |n: usize| {
            (0..n)
                .map(|i| {
                    format!(
                        r#"{{"created":"2024-01-02T03:04:05.{i:09}Z","created_by":"RUN /bin/sh -c step {i} && make install","comment":"buildkit.dockerfile.v0"{}}}"#,
                        if i % 3 == 0 { r#","empty_layer":true"# } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let config = |steps: usize, env: usize| {
            let env: Vec<String> = (0..env).map(|i| format!(r#""VAR_{i}=value-{i}""#)).collect();
            let labels: Vec<String> = (0..env.len())
                .map(|i| format!(r#""org.example.label.{i}":"{i}""#))
                .collect();
            let diff_ids: Vec<String> = (0..steps.div_ceil(3))
                .map(|i| format!(r#""sha256:{i:064x}""#))
                .collect();
            format!(
                r#"{{"architecture":"arm64","os":"linux","created":"2024-01-02T03:04:05Z","config":{{"User":"app","Env":[{}],"Entrypoint":["/bin/app"],"Cmd":["serve"],"WorkingDir":"/srv","Labels":{{{}}},"ExposedPorts":{{"80/tcp":{{}},"443/tcp":{{}}}},"Volumes":{{"/data":{{}}}},"StopSignal":"SIGTERM"}},"rootfs":{{"type":"layers","diff_ids":[{}]}},"history":[{}]}}"#,
                env.join(","),
                labels.join(","),
                diff_ids.join(","),
                history(steps)
            )
        };
        let quantiles = |mut took: Vec<u128>| {
            took.sort_unstable();
            let q = |p: usize| took[(took.len() * p / 100).min(took.len() - 1)];
            format!(
                "p50 {} p90 {} p99 {} max {}",
                q(50),
                q(90),
                q(99),
                took[took.len() - 1]
            )
        };
        for (name, text, n) in [
            ("build-sized", config(12, 8), 2000),
            ("long history", config(2000, 500), 200),
            ("near MAX_CONFIG", config(18000, 4000), 40),
        ] {
            assert!(text.len() as u64 <= MAX_CONFIG, "{name}: {}", text.len());
            assert!(parse_config(text.as_bytes()).is_ok(), "{name}");
            assert!(serde_reader::parse_config(text.as_bytes()).is_ok(), "{name}");
            let (mut go, mut serde) = (Vec::with_capacity(n), Vec::with_capacity(n));
            for i in 0..n {
                // Interleaved, each first in turn, so a busy host weighs on both alike.
                for which in [i % 2, 1 - i % 2] {
                    let at = std::time::Instant::now();
                    if which == 0 {
                        std::hint::black_box(parse_config(std::hint::black_box(text.as_bytes()))).unwrap();
                        go.push(at.elapsed().as_micros());
                    } else {
                        std::hint::black_box(serde_reader::parse_config(std::hint::black_box(
                            text.as_bytes(),
                        )))
                        .unwrap();
                        serde.push(at.elapsed().as_micros());
                    }
                }
            }
            eprintln!("{name}: {} bytes, n={n}", text.len());
            eprintln!("{name}: go {}", quantiles(go));
            eprintln!("{name}: serde {}", quantiles(serde));
        }
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
