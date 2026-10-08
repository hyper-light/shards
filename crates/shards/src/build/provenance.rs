//! The SLSA provenance attestation a build's image carries, as Docker's BuildKit (v0.28.1,
//! Docker 29.3.1) makes it in `mode=min`, the mode buildx asks for by default
//! (`attest:provenance=mode=min,inline-only=true`): the statement (an in-toto Statement
//! v0.1 of a SLSA v1 predicate, solver/llbsolver/provenance), the attestation manifest and
//! its config that hold it, and the index of the image and its attestation that the
//! image's ID names (exporter/containerimage); and the v0.2 form buildx writes in the
//! metadata file (D71).

use std::collections::BTreeMap;

use shards_dockerfile::export::json_string;

/// A JSON value, written as Go's encoding/json writes the structs BuildKit marshals:
/// fields in the order given, strings HTML-escaped.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Str(String),
    Bool(bool),
    Int(i64),
    Obj(Vec<(String, Json)>),
    Arr(Vec<Json>),
}

impl Json {
    fn s(v: impl Into<String>) -> Json {
        Json::Str(v.into())
    }

    fn obj(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// `json.Marshal`.
    pub fn compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// `json.MarshalIndent(v, "", "  ")`, its first line at `depth`.
    pub fn indented(&self, depth: usize) -> String {
        let mut out = String::new();
        self.write(&mut out, Some("  "), depth);
        out
    }

    fn write(&self, out: &mut String, step: Option<&str>, depth: usize) {
        let newline = |out: &mut String, depth: usize| {
            if let Some(step) = step {
                out.push('\n');
                for _ in 0..depth {
                    out.push_str(step);
                }
            }
        };
        match self {
            Json::Str(s) => out.push_str(&json_string(s.as_bytes())),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Obj(fields) if fields.is_empty() => out.push_str("{}"),
            Json::Arr(items) if items.is_empty() => out.push_str("[]"),
            Json::Obj(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    out.push_str(&json_string(k.as_bytes()));
                    out.push(':');
                    if step.is_some() {
                        out.push(' ');
                    }
                    v.write(out, step, depth + 1);
                }
                newline(out, depth);
                out.push('}');
            }
            Json::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    v.write(out, step, depth + 1);
                }
                newline(out, depth);
                out.push(']');
            }
        }
    }
}

/// A material (`slsa.ProvenanceMaterial`): what the build read, and its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Material {
    pub uri: String,
    pub algorithm: String,
    pub hex: String,
}

/// What BuildKit captures of a build for its provenance (`provenance.Capture`): the
/// frontend's options as buildx sent them, its sources, the locals it read, and the
/// secrets and SSH agents its steps mounted.
#[derive(Debug, Clone, Default)]
pub struct Capture {
    pub args: BTreeMap<String, String>,
    pub materials: Vec<Material>,
    pub locals: Vec<String>,
    pub secrets: Vec<(String, bool)>,
    pub ssh: Vec<(String, bool)>,
    /// Whether any step had the network, which makes a build not hermetic.
    pub network: bool,
}

/// The run's own facts: its invocation's ID (the build's reference), when it started and
/// finished (seconds and nanoseconds), the builder's platform and ID.
#[derive(Debug, Clone)]
pub struct Run {
    pub invocation_id: String,
    pub started: (i64, u32),
    pub finished: (i64, u32),
    pub builder_platform: String,
    pub builder_id: String,
    /// `reproducible=true`, as the request said it.
    pub reproducible: bool,
}

pub const BUILD_TYPE_V1: &str =
    "https://github.com/moby/buildkit/blob/master/docs/attestations/slsa-definitions.md";
pub const BUILD_TYPE_V02: &str = "https://mobyproject.org/buildkit@v1";
pub const PREDICATE_TYPE: &str = "https://slsa.dev/provenance/v1";
pub const IN_TOTO: &str = "application/vnd.in-toto+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";

/// `purl.RefToPURL(docker, ref, platform)` (util/purl): an image as a package URL: its
/// familiar name's path as namespace and name, its tag as the version (`latest` where it
/// has neither tag nor digest), a digest as a qualifier, and the platform, normalized.
pub fn image_purl(reference: &shards_image::reference::Reference, platform: &str) -> String {
    let familiar = reference.familiar();
    // The familiar name alone: no tag, no digest.
    let name = familiar
        .split('@')
        .next()
        .map(|n| match n.rfind(':') {
            Some(i) if !n.get(i..).unwrap_or_default().contains('/') => n.get(..i).unwrap_or(n),
            _ => n,
        })
        .unwrap_or_default();
    let (namespace, base) = match name.rfind('/') {
        Some(i) => (
            name.get(..i).unwrap_or_default(),
            name.get(i + 1..).unwrap_or_default(),
        ),
        None => ("", name),
    };
    let mut out = String::from("pkg:docker/");
    if !namespace.is_empty() {
        for (i, seg) in namespace.split('/').enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(&escape(seg, b""));
        }
        out.push('/');
    }
    out.push_str(&escape(base, b""));
    let mut qualifiers: Vec<(&str, String)> = Vec::new();
    match (&reference.digest, &reference.tag) {
        (Some(d), _) => qualifiers.push(("digest", d.to_string())),
        (None, tag) => {
            out.push('@');
            out.push_str(&escape(tag.as_deref().unwrap_or("latest"), b""));
        }
    }
    if !platform.is_empty() {
        qualifiers.push(("platform", platform.to_string()));
    }
    qualifiers.sort_by(|a, b| a.0.cmp(b.0));
    for (i, (k, v)) in qualifiers.iter().enumerate() {
        out.push(if i == 0 { '?' } else { '&' });
        out.push_str(k);
        out.push('=');
        out.push_str(&escape(v, b":"));
    }
    out
}

/// Percent-encoding as package-url writes a part: every byte but the unreserved ones
/// and those `keep` names.
fn escape(s: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || keep.contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Secrets by ID, each whether it is optional; SSH agents the same; and whether any step
/// has the network.
pub type Mounts = (Vec<(String, bool)>, Vec<(String, bool)>, bool);

/// The secrets and SSH agents the build's steps mount, and whether any step has the
/// network, as BuildKit captures them (captureProvenance, Capture.AddSecret and AddSSH:
/// one optional only where every mount of it is, each list sorted by ID); a Git source's
/// credentials and SSH agent too (GitIdentifier.Capture), each optional.
pub fn capture_mounts(def: &shards_dockerfile::llb::Definition) -> Mounts {
    use shards_dockerfile::llb::{NetMode, OpKind, OpMountKind};
    let add = |list: &mut Vec<(String, bool)>, id: &[u8], optional: bool| {
        let id = String::from_utf8_lossy(id).into_owned();
        match list.iter_mut().find(|(i, _)| *i == id) {
            Some((_, o)) => *o = *o && optional,
            None => list.push((id, optional)),
        }
    };
    let (mut secrets, mut ssh, mut network) = (Vec::new(), Vec::new(), false);
    for op in &def.ops {
        match &op.kind {
            OpKind::Exec {
                mounts,
                network: net,
                secret_env,
                ..
            } => {
                for m in mounts {
                    match &m.kind {
                        OpMountKind::Secret { id, optional, .. } => add(&mut secrets, id, *optional),
                        // An SSH mount with no ID is the default agent's (llb's SSHOpt).
                        OpMountKind::Ssh { id, optional, .. } => {
                            let id: &[u8] = if id.is_empty() { b"default" } else { id };
                            add(&mut ssh, id, *optional)
                        }
                        _ => {}
                    }
                }
                for (id, _, optional) in secret_env {
                    add(&mut secrets, id, *optional);
                }
                network |= *net != NetMode::None;
            }
            OpKind::Source { identifier, attrs } if identifier.starts_with(b"git://") => {
                for key in [b"git.authtokensecret".as_slice(), b"git.authheadersecret"] {
                    if let Some(id) = attrs.get(key) {
                        add(&mut secrets, id, true);
                    }
                }
                if let Some(id) = attrs.get(b"git.mountsshsock".as_slice()) {
                    add(&mut ssh, id, true);
                }
            }
            _ => {}
        }
    }
    secrets.sort();
    ssh.sort();
    (secrets, ssh, network)
}

/// The base images the build read (ImageIdentifier.Capture): each source's reference,
/// its pin (the digest it resolved to) dropped where it has a tag, its platform; each
/// once, by reference.
pub fn capture_images(def: &shards_dockerfile::llb::Definition) -> Vec<Material> {
    use shards_dockerfile::llb::OpKind;
    let mut out: Vec<(String, Material)> = Vec::new();
    for op in &def.ops {
        let OpKind::Source { identifier, .. } = &op.kind else {
            continue;
        };
        let Some(rest) = identifier.strip_prefix(b"docker-image://") else {
            continue;
        };
        let Ok(mut reference) = shards_image::reference::Reference::parse(&String::from_utf8_lossy(rest))
        else {
            continue;
        };
        let Some(pin) = reference.digest.clone() else {
            continue;
        };
        if reference.tag.is_some() {
            reference.digest = None;
        }
        let platform = op
            .platform
            .as_ref()
            .map(|p| String::from_utf8_lossy(&shards_dockerfile::platform::format(p)).into_owned())
            .unwrap_or_default();
        let key = format!("{reference} {platform}");
        if out.iter().any(|(k, _)| *k == key) {
            continue;
        }
        let pin = pin.to_string();
        let (algorithm, hex) = pin.split_once(':').unwrap_or(("sha256", pin.as_str()));
        out.push((
            reference.to_string(),
            Material {
                uri: image_purl(&reference, &platform),
                algorithm: algorithm.to_string(),
                hex: hex.to_string(),
            },
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|(_, m)| m).collect()
}

/// `provenance.FilterArgs`: the frontend's options without those of this host (cgroup
/// parent, image resolve mode, platform, cache imports) or the attestations asked for.
fn filter_args(args: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    const HOST: [&str; 4] = ["cgroup-parent", "image-resolve-mode", "platform", "cache-imports"];
    args.iter()
        .filter(|(k, _)| !HOST.contains(&k.as_str()) && !k.starts_with("attest:"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Go's `time.Time` as `MarshalJSON` writes it: RFC 3339 in UTC, its nanoseconds with
/// no trailing zeros.
fn rfc3339_nano((secs, nanos): (i64, u32)) -> String {
    let mut t = shards_dockerfile::go::Time::from_unix(secs);
    t.nanosecond = nanos;
    t.rfc3339_nano().unwrap_or_default()
}

fn digest_set(m: &Material) -> Json {
    Json::Obj(vec![(m.algorithm.clone(), Json::s(m.hex.clone()))])
}

fn locals(c: &Capture) -> Json {
    Json::Arr(
        c.locals
            .iter()
            .map(|l| Json::obj(vec![("name", Json::s(l.clone()))]))
            .collect(),
    )
}

/// `NewPredicate` then `mode=min` (NewProvenanceCreator): the SLSA v1 predicate.
pub fn predicate(c: &Capture, run: &Run) -> Json {
    let mut args = filter_args(&c.args);
    let filename = args.remove("filename").filter(|f| !f.is_empty());
    // mode=min: build arguments and labels left out, and the request said incomplete.
    let mut complete_request = true;
    args.retain(|k, _| {
        let dropped = k.starts_with("build-arg:") || k.starts_with("label:");
        if dropped {
            complete_request = false;
        }
        !dropped
    });
    let mut config_source = Vec::new();
    if let Some(f) = filename {
        config_source.push(("path", Json::s(f)));
    }
    let mut request = vec![("frontend", Json::s("dockerfile.v0"))];
    if !args.is_empty() {
        request.push((
            "args",
            Json::Obj(args.into_iter().map(|(k, v)| (k, Json::s(v))).collect()),
        ));
    }
    if !c.locals.is_empty() {
        request.push(("locals", locals(c)));
    }
    let mut definition = vec![("buildType", Json::s(BUILD_TYPE_V1))];
    if !c.materials.is_empty() {
        definition.push((
            "resolvedDependencies",
            Json::Arr(
                c.materials
                    .iter()
                    .map(|m| Json::obj(vec![("uri", Json::s(m.uri.clone())), ("digest", digest_set(m))]))
                    .collect(),
            ),
        ));
    }
    definition.push((
        "externalParameters",
        Json::obj(vec![
            ("configSource", Json::obj(config_source)),
            ("request", Json::obj(request)),
        ]),
    ));
    definition.push((
        "internalParameters",
        Json::obj(vec![("builderPlatform", Json::s(run.builder_platform.clone()))]),
    ));
    // Locals are materials no digest pins: never complete, so never hermetic.
    let complete_materials = c.locals.is_empty();
    let mut metadata = vec![
        ("invocationId", Json::s(run.invocation_id.clone())),
        ("startedOn", Json::s(rfc3339_nano(run.started))),
        ("finishedOn", Json::s(rfc3339_nano(run.finished))),
        ("buildkit_metadata", Json::Obj(Vec::new())),
    ];
    if complete_materials && !c.network {
        metadata.push(("buildkit_hermetic", Json::Bool(true)));
    }
    metadata.push((
        "buildkit_completeness",
        Json::obj(vec![
            ("request", Json::Bool(complete_request)),
            ("resolvedDependencies", Json::Bool(complete_materials)),
        ]),
    ));
    if run.reproducible {
        metadata.push(("buildkit_reproducible", Json::Bool(true)));
    }
    Json::obj(vec![
        ("buildDefinition", Json::obj(definition)),
        (
            "runDetails",
            Json::obj(vec![
                (
                    "builder",
                    Json::obj(vec![("id", Json::s(run.builder_id.clone()))]),
                ),
                ("metadata", Json::obj(metadata)),
            ]),
        ),
    ])
}

/// The in-toto statement the attestation layer holds. Its subjects are the image's
/// names, each as a package URL with the image's platform and the manifest's digest, as
/// BuildKit's image exporter names a stored or pushed image; none for an image of no name
/// (an OCI or docker archive's), which the index's reference names alone.
pub fn statement(c: &Capture, run: &Run, subjects: &[(String, String)]) -> String {
    statement_json(c, run, subjects).compact()
}

/// [`statement`] as a value: compact in an image's attestation, indented in a local
/// output's `provenance.json`.
pub fn statement_json(c: &Capture, run: &Run, subjects: &[(String, String)]) -> Json {
    let subjects = subjects
        .iter()
        .map(|(name, digest)| {
            let (alg, hex) = digest.split_once(':').unwrap_or(("sha256", digest.as_str()));
            Json::obj(vec![
                ("name", Json::s(name.clone())),
                ("digest", Json::Obj(vec![(alg.to_string(), Json::s(hex))])),
            ])
        })
        .collect();
    Json::obj(vec![
        ("_type", Json::s("https://in-toto.io/Statement/v0.1")),
        ("predicateType", Json::s(PREDICATE_TYPE)),
        ("subject", Json::Arr(subjects)),
        ("predicate", predicate(c, run)),
    ])
}

/// The v0.2 provenance buildx writes in the metadata file (`buildx.build.provenance`):
/// the build's, every argument but this host's, without its run's metadata.
pub fn buildinfo(c: &Capture, run: &Run) -> Json {
    let mut args = filter_args(&c.args);
    let filename = args.remove("filename").filter(|f| !f.is_empty());
    let mut fields = vec![
        (
            "builder",
            Json::obj(vec![("id", Json::s(run.builder_id.clone()))]),
        ),
        ("buildType", Json::s(BUILD_TYPE_V02)),
    ];
    if !c.materials.is_empty() {
        fields.push((
            "materials",
            Json::Arr(
                c.materials
                    .iter()
                    .map(|m| Json::obj(vec![("uri", Json::s(m.uri.clone())), ("digest", digest_set(m))]))
                    .collect(),
            ),
        ));
    }
    let mut config_source = Vec::new();
    if let Some(f) = filename {
        config_source.push(("entryPoint", Json::s(f)));
    }
    let mut parameters = vec![("frontend", Json::s("dockerfile.v0"))];
    if !args.is_empty() {
        parameters.push((
            "args",
            Json::Obj(args.into_iter().map(|(k, v)| (k, Json::s(v))).collect()),
        ));
    }
    let mounts = |list: &[(String, bool)]| {
        let mut sorted = list.to_vec();
        sorted.sort();
        Json::Arr(
            sorted
                .into_iter()
                .map(|(id, optional)| {
                    let mut f = vec![("id", Json::s(id))];
                    if optional {
                        f.push(("optional", Json::Bool(true)));
                    }
                    Json::obj(f)
                })
                .collect(),
        )
    };
    if !c.secrets.is_empty() {
        parameters.push(("secrets", mounts(&c.secrets)));
    }
    if !c.ssh.is_empty() {
        parameters.push(("ssh", mounts(&c.ssh)));
    }
    if !c.locals.is_empty() {
        parameters.push(("locals", locals(c)));
    }
    fields.push((
        "invocation",
        Json::obj(vec![
            ("configSource", Json::obj(config_source)),
            ("parameters", Json::obj(parameters)),
            (
                "environment",
                Json::obj(vec![("platform", Json::s(run.builder_platform.clone()))]),
            ),
        ]),
    ));
    Json::obj(fields)
}

/// A descriptor as ocispec's Go struct marshals it: media type, digest, size, then
/// annotations and platform where given.
fn descriptor(
    media: &str,
    digest: &str,
    size: usize,
    annotations: &[(&str, &str)],
    platform: Option<Json>,
) -> Json {
    let mut f = vec![
        ("mediaType", Json::s(media)),
        ("digest", Json::s(digest)),
        ("size", Json::Int(i64::try_from(size).unwrap_or(i64::MAX))),
    ];
    if !annotations.is_empty() {
        f.push((
            "annotations",
            Json::Obj(
                annotations
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), Json::s(*v)))
                    .collect(),
            ),
        ));
    }
    if let Some(p) = platform {
        f.push(("platform", p));
    }
    Json::obj(f)
}

/// What holds a statement: its config (an image config of no platform whose one diff ID
/// is the statement's own digest) and its manifest, as BuildKit's exporter writes them.
pub fn attestation(statement_digest: &str, statement_size: usize) -> (String, String) {
    let config = Json::obj(vec![
        ("architecture", Json::s("unknown")),
        ("os", Json::s("unknown")),
        ("config", Json::Obj(Vec::new())),
        (
            "rootfs",
            Json::obj(vec![
                ("type", Json::s("layers")),
                ("diff_ids", Json::Arr(vec![Json::s(statement_digest)])),
            ]),
        ),
    ])
    .compact();
    let config_digest = sha256(config.as_bytes());
    let manifest = Json::obj(vec![
        ("schemaVersion", Json::Int(2)),
        ("mediaType", Json::s(shards_image::oci::media::OCI_MANIFEST)),
        (
            "config",
            descriptor(OCI_CONFIG, &config_digest, config.len(), &[], None),
        ),
        (
            "layers",
            Json::Arr(vec![descriptor(
                IN_TOTO,
                statement_digest,
                statement_size,
                &[("in-toto.io/predicate-type", PREDICATE_TYPE)],
                None,
            )]),
        ),
    ])
    .indented(0);
    (config, manifest)
}

/// The index of the image and its attestation, which the image's ID names.
pub fn index(image: (&str, usize), platform: (&str, &str, &str), attestation: (&str, usize)) -> String {
    index_of(
        &[(image.0, image.1, platform)],
        &[(attestation.0, attestation.1, image.0)],
    )
}

/// The index of an image's `manifests` (digest, size, platform: architecture, OS,
/// variant), then their `attestations` (digest, size, and the manifest each attests), as
/// BuildKit's exporter writes it, platforms first (measured: Docker 29.3.1, D77).
pub fn index_of(
    manifests: &[(&str, usize, (&str, &str, &str))],
    attestations: &[(&str, usize, &str)],
) -> String {
    let mut entries = Vec::new();
    for &(digest, size, (arch, os, variant)) in manifests {
        let mut p = vec![("architecture", Json::s(arch)), ("os", Json::s(os))];
        if !variant.is_empty() {
            p.push(("variant", Json::s(variant)));
        }
        entries.push(descriptor(
            shards_image::oci::media::OCI_MANIFEST,
            digest,
            size,
            &[],
            Some(Json::obj(p)),
        ));
    }
    for &(digest, size, of) in attestations {
        entries.push(descriptor(
            shards_image::oci::media::OCI_MANIFEST,
            digest,
            size,
            &[
                ("vnd.docker.reference.digest", of),
                ("vnd.docker.reference.type", "attestation-manifest"),
            ],
            Some(Json::obj(vec![
                ("architecture", Json::s("unknown")),
                ("os", Json::s("unknown")),
            ])),
        ));
    }
    Json::obj(vec![
        ("schemaVersion", Json::Int(2)),
        ("mediaType", Json::s(shards_image::oci::media::OCI_INDEX)),
        ("manifests", Json::Arr(entries)),
    ])
    .indented(0)
}

fn sha256(b: &[u8]) -> String {
    use sha2::Digest as _;
    let d = sha2::Sha256::digest(b);
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        name: String,
        flags: Vec<String>,
        #[serde(default)]
        dockerfile: Option<String>,
        #[serde(default)]
        statement: Option<String>,
        #[serde(default)]
        attestation_manifest: Option<String>,
        #[serde(default)]
        attestation_config: Option<String>,
        #[serde(default)]
        index: Option<String>,
        #[serde(default)]
        metadata: Option<String>,
        #[serde(default)]
        localprov: Option<String>,
    }

    /// Any image, resolved to busybox's digest with an empty config, for planning.
    struct AnyImage;

    impl shards_dockerfile::plan::Resolver for AnyImage {
        fn resolve(
            &self,
            name: &[u8],
            _: &shards_dockerfile::platform::Platform,
            _: &[u8],
        ) -> Result<shards_dockerfile::plan::Resolved, Vec<u8>> {
            let mut reference = name.to_vec();
            if !reference.contains(&b'@') {
                reference.extend_from_slice(
                    b"@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662",
                );
            }
            Ok(shards_dockerfile::plan::Resolved {
                reference,
                digest: Some(b"sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662".to_vec()),
                config: br#"{"architecture":"arm64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec(),
            })
        }

        fn epoch(&self, _: &shards_dockerfile::plan::EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
            Ok(None)
        }
    }

    fn plan_of(dockerfile: &str) -> shards_dockerfile::llb::Definition {
        let opts = shards_dockerfile::plan::Options {
            target_platform: shards_dockerfile::platform::Platform::new("linux", "arm64"),
            build_platforms: vec![shards_dockerfile::platform::Platform::new("linux", "arm64")],
            ..Default::default()
        };
        shards_dockerfile::plan::plan(dockerfile.as_bytes(), &opts, &AnyImage)
            .unwrap()
            .definition()
    }

    fn cases() -> Vec<Case> {
        serde_json::from_str(include_str!("testdata/provenance.json")).unwrap()
    }

    /// The run's facts and materials, as the recorded statement has them: what the
    /// builder alone decides (its time, its ID, what it resolved).
    fn run_and_materials(st: &serde_json::Value) -> (Run, Vec<Material>) {
        let p = &st["predicate"];
        let md = &p["runDetails"]["metadata"];
        let time = |v: &serde_json::Value| -> (i64, u32) {
            shards_dockerfile::go::parse_rfc3339(v.as_str().unwrap().as_bytes())
                .unwrap()
                .unix()
        };
        let materials = p["buildDefinition"]["resolvedDependencies"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|m| {
                        let (alg, hex) = m["digest"].as_object().unwrap().iter().next().unwrap();
                        Material {
                            uri: m["uri"].as_str().unwrap().to_string(),
                            algorithm: alg.clone(),
                            hex: hex.as_str().unwrap().to_string(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        (
            Run {
                invocation_id: md["invocationId"].as_str().unwrap().to_string(),
                started: time(&md["startedOn"]),
                finished: time(&md["finishedOn"]),
                builder_platform: p["buildDefinition"]["internalParameters"]["builderPlatform"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                builder_id: p["runDetails"]["builder"]["id"].as_str().unwrap().to_string(),
                reproducible: md["buildkit_reproducible"].as_bool().unwrap_or(false),
            },
            materials,
        )
    }

    /// Each recorded statement, given the frontend's options buildx sent for its flags
    /// (`super::super::buildx_attrs`), the locals, and what the builder alone decided;
    /// then the attestation's config and manifest, and the index, byte for byte.
    #[test]
    fn provenance_is_buildkits() {
        let all = cases();
        assert!(all.len() >= 12);
        let mut checked = 0;
        // A local output's provenance.json: the statement, indented, its files its subjects.
        let local = all.iter().find(|c| c.name == "local-output").unwrap();
        let text = local.localprov.as_ref().unwrap();
        let recorded: serde_json::Value = serde_json::from_str(text).unwrap();
        let (run, materials) = run_and_materials(&recorded);
        let subjects: Vec<(String, String)> = recorded["subject"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    format!("sha256:{}", s["digest"]["sha256"].as_str().unwrap()),
                )
            })
            .collect();
        let capture = Capture {
            args: super::super::buildx_attrs_of(&local.flags).unwrap(),
            materials,
            locals: vec!["context".into(), "dockerfile".into()],
            ..Capture::default()
        };
        assert_eq!(statement_json(&capture, &run, &subjects).indented(0), *text);
        for case in &all {
            let Some(st) = &case.statement else { continue };
            let recorded: serde_json::Value = serde_json::from_str(st).unwrap();
            let (run, materials) = run_and_materials(&recorded);
            let args = super::super::buildx_attrs_of(&case.flags).unwrap();
            let named: Vec<String> = case
                .flags
                .iter()
                .filter_map(|f| f.split_once('=').map(|(n, _)| n.to_string()))
                .filter(|_| case.flags.iter().any(|f| f == "--build-context"))
                .collect();
            let mut locals = vec!["context".to_string(), "dockerfile".to_string()];
            locals.extend(named.into_iter().filter(|n| !n.starts_with("--")));
            locals.sort();
            let (secrets, ssh, network) = capture_mounts(&plan_of(case.dockerfile.as_ref().unwrap()));
            let mut capture = Capture {
                args,
                materials,
                locals,
                secrets,
                ssh,
                network,
            };
            // The shared key's node ID is the recording builder's.
            for (k, v) in capture.args.iter_mut() {
                if k.starts_with("sharedkey:localdir:") {
                    let want =
                        recorded["predicate"]["buildDefinition"]["externalParameters"]["request"]["args"][k]
                            .as_str()
                            .unwrap()
                            .to_string();
                    *v = want;
                }
            }
            // The subjects: each name the build was given (`-t`), with the manifest's digest
            // the recording names (the builder's own).
            let mut subjects = Vec::new();
            if let Some(i) = case.flags.iter().position(|f| f == "-t") {
                let name = shards_image::reference::Reference::parse(&case.flags[i + 1]).unwrap();
                let digest = recorded["subject"][0]["digest"]["sha256"].as_str().unwrap();
                subjects.push((
                    image_purl(&name, &run.builder_platform),
                    format!("sha256:{digest}"),
                ));
            }
            let ours = statement(&capture, &run, &subjects);
            assert_eq!(ours, *st, "{}", case.name);
            let digest = sha256(st.as_bytes());
            let (config, manifest) = attestation(&digest, st.len());
            assert_eq!(Some(&config), case.attestation_config.as_ref(), "{}", case.name);
            assert_eq!(
                Some(&manifest),
                case.attestation_manifest.as_ref(),
                "{}",
                case.name
            );
            // The index: the image's manifest as the recorded one names it.
            let idx: serde_json::Value = serde_json::from_str(case.index.as_ref().unwrap()).unwrap();
            let image = &idx["manifests"][0];
            let ours = index(
                (
                    image["digest"].as_str().unwrap(),
                    image["size"].as_u64().unwrap() as usize,
                ),
                (
                    image["platform"]["architecture"].as_str().unwrap(),
                    image["platform"]["os"].as_str().unwrap(),
                    image["platform"]["variant"].as_str().unwrap_or(""),
                ),
                (&sha256(manifest.as_bytes()), manifest.len()),
            );
            assert_eq!(Some(&ours), case.index.as_ref(), "{}", case.name);
            // A stored image's metadata file, whole: the index its name resolves to, the
            // provenance, the name; buildx's builder in the reference is Docker's.
            if case.name == "stored-default" {
                let recorded_md = case.metadata.as_ref().unwrap();
                let md: serde_json::Value = serde_json::from_str(recorded_md).unwrap();
                let index_digest =
                    shards_image::reference::Digest::parse(md["containerimage.digest"].as_str().unwrap())
                        .unwrap();
                let id = md["buildx.build.ref"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap();
                let ours = super::super::metadata(
                    id,
                    Some((
                        &index_digest,
                        md["containerimage.descriptor"]["size"].as_u64().unwrap() as usize,
                        &[md["image.name"].as_str().unwrap().to_string()],
                    )),
                    &BTreeMap::new(),
                    &[(None, &buildinfo(&capture, &run))],
                    shards_image::oci::media::OCI_INDEX,
                );
                assert_eq!(ours.replace("shards/shards/", "default/default/"), *recorded_md);
            }
            // buildx's metadata file: its v0.2 provenance.
            let md: serde_json::Value = serde_json::from_str(case.metadata.as_ref().unwrap()).unwrap();
            let want = serde_json::to_string_pretty(&md["buildx.build.provenance"]).unwrap();
            let got: serde_json::Value = serde_json::from_str(&buildinfo(&capture, &run).compact()).unwrap();
            assert_eq!(serde_json::to_string_pretty(&got).unwrap(), want, "{}", case.name);
            checked += 1;
        }
        assert!(checked >= 10, "{checked}");
    }

    #[test]
    fn images_are_package_urls_as_buildkit_writes_them() {
        use shards_image::reference::Reference;
        let p = |r: &str| image_purl(&Reference::parse(r).unwrap(), "linux/arm64");
        assert_eq!(
            p("busybox:1.36"),
            "pkg:docker/busybox@1.36?platform=linux%2Farm64"
        );
        assert_eq!(p("busybox"), "pkg:docker/busybox@latest?platform=linux%2Farm64");
        assert_eq!(
            p("gcr.io/distroless/static-debian12:nonroot"),
            "pkg:docker/gcr.io/distroless/static-debian12@nonroot?platform=linux%2Farm64"
        );
        assert_eq!(
            p("busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662"),
            "pkg:docker/busybox?digest=sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662&platform=linux%2Farm64"
        );
        // A registry's port, escaped (the oracle's base-port case).
        assert_eq!(
            p("localhost:5000/team/busybox:1.36"),
            "pkg:docker/localhost%3A5000/team/busybox@1.36?platform=linux%2Farm64"
        );
    }
}
