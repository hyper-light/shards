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
    /// `mode=max`'s records, where it was asked for.
    pub max: Option<Max>,
    /// The SBOMs the build's scanner wrote (D81), the core target's first.
    pub sboms: Vec<super::sbom::Scanned>,
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
pub fn capture_images(
    def: &shards_dockerfile::llb::Definition,
    unplatformed: Option<&[u8]>,
) -> Vec<Material> {
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
        // An SBOM's scanner, resolved with no platform (`CreateSBOMScanner`), says none.
        let platform = op
            .platform
            .as_ref()
            .filter(|_| unplatformed != Some(identifier.as_slice()))
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
    // mode=min: build arguments and labels left out, and the request said incomplete;
    // mode=max keeps them, and the secrets and SSH agents the steps mounted.
    let mut complete_request = true;
    if c.max.is_none() {
        args.retain(|k, _| {
            let dropped = k.starts_with("build-arg:") || k.starts_with("label:");
            if dropped {
                complete_request = false;
            }
            !dropped
        });
    }
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
    if c.max.is_some() {
        let mounted = |list: &[(String, bool)]| {
            Json::Arr(
                list.iter()
                    .map(|(id, optional)| {
                        let mut f = vec![("id", Json::s(id.clone()))];
                        if *optional {
                            f.push(("optional", Json::Bool(true)));
                        }
                        Json::obj(f)
                    })
                    .collect(),
            )
        };
        if !c.secrets.is_empty() {
            request.push(("secrets", mounted(&c.secrets)));
        }
        if !c.ssh.is_empty() {
            request.push(("ssh", mounted(&c.ssh)));
        }
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
    // Written through a map (ProvenanceInternalParametersSLSA1.MarshalJSON): keys in order.
    let mut internal = Vec::new();
    if let Some(m) = &c.max {
        internal.push(("buildConfig", m.build_config.clone()));
    }
    internal.push(("builderPlatform", Json::s(run.builder_platform.clone())));
    definition.push(("internalParameters", Json::obj(internal)));
    // Locals are materials no digest pins: never complete, so never hermetic.
    let complete_materials = c.locals.is_empty();
    let mut metadata = vec![
        ("invocationId", Json::s(run.invocation_id.clone())),
        ("startedOn", Json::s(rfc3339_nano(run.started))),
        ("finishedOn", Json::s(rfc3339_nano(run.finished))),
        ("buildkit_metadata", buildkit_metadata(c.max.as_ref())),
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

/// The statements an image's attestation holds, each with its predicate type: its SBOMs,
/// then its provenance where it carries one (exporter/attestation `Unbundle`'s order,
/// then the solver's provenance).
pub fn image_statements(
    c: &Capture,
    run: &Run,
    subjects: &[(String, String)],
    provenance: bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = c
        .sboms
        .iter()
        .map(|s| (super::sbom::intoto(s, subjects), s.predicate_type.clone()))
        .collect();
    if provenance {
        out.push((statement(c, run, subjects), PREDICATE_TYPE.to_string()));
    }
    out
}

/// `BuildKitMetadata`: for `mode=max`, its source map and its steps' layers.
fn buildkit_metadata(max: Option<&Max>) -> Json {
    let Some(m) = max else {
        return Json::Obj(Vec::new());
    };
    let mut f = vec![("source", m.source.clone())];
    if !m.layers.is_empty() {
        f.push(("layers", Json::Obj(m.layers.clone())));
    }
    Json::obj(f)
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

/// The attestation manifest of several statements (each its digest, size and predicate
/// type), in the order given, and its config, whose diff IDs they are: an image's SBOMs,
/// then its provenance (D81).
pub fn attestation_of(statements: &[(&str, usize, &str)]) -> (String, String) {
    let config = Json::obj(vec![
        ("architecture", Json::s("unknown")),
        ("os", Json::s("unknown")),
        ("config", Json::Obj(Vec::new())),
        (
            "rootfs",
            Json::obj(vec![
                ("type", Json::s("layers")),
                (
                    "diff_ids",
                    Json::Arr(statements.iter().map(|(d, _, _)| Json::s(*d)).collect()),
                ),
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
            Json::Arr(
                statements
                    .iter()
                    .map(|(d, size, kind)| {
                        descriptor(IN_TOTO, d, *size, &[("in-toto.io/predicate-type", kind)], None)
                    })
                    .collect(),
            ),
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

/// What `mode=max` adds to a build's provenance (D80): its LLB definition as steps
/// (`buildConfig`), where in the Dockerfile each step comes from (`source`), and the
/// layers each step's output is (`layers`).
#[derive(Debug, Clone, PartialEq)]
pub struct Max {
    pub build_config: Json,
    pub source: Json,
    /// Each step's output (`stepN:I`) and its chain of layers, in key order.
    pub layers: Vec<(String, Json)>,
}

/// A layer as the cache exporter describes it: its media type, digest and size.
pub fn layer_descriptor(media_type: &str, digest: &str, size: u64) -> Json {
    Json::obj(vec![
        ("mediaType", Json::s(media_type)),
        ("digest", Json::s(digest)),
        ("size", Json::Int(i64::try_from(size).unwrap_or(i64::MAX))),
    ])
}

/// `toBuildSteps` (solver/llbsolver/provenance.go): `def`'s ops from its last back, each
/// after its inputs, each a step named by its place, its inputs named `stepN:I`, a local
/// source's session and unique ID left out; the digest of each op's bytes to its step; and
/// each op's step, by its index in `def`.
pub struct Steps {
    pub steps: Vec<Json>,
    pub mapping: Vec<(String, String)>,
    pub step_of: Vec<Option<usize>>,
}

pub fn steps(def: &shards_dockerfile::llb::Definition) -> Steps {
    // Each op's digest, its inputs before it (the marshal's order).
    let mut digests: Vec<Vec<u8>> = Vec::with_capacity(def.ops.len());
    for op in &def.ops {
        let inputs: Vec<Vec<u8>> = op
            .inputs
            .iter()
            .map(|i| digests.get(i.op).cloned().unwrap_or_default())
            .collect();
        let bytes = shards_dockerfile::pb::op(op, &inputs).unwrap_or_default();
        digests.push(sha256(&bytes).into_bytes());
    }
    let mut out = Steps {
        steps: Vec::new(),
        mapping: Vec::new(),
        step_of: vec![None; def.ops.len()],
    };
    let Some(root) = def.root else {
        return out;
    };
    // walkDigests from the last op, the root, which names `root`: depth first, each input
    // before what reads it, without recursion.
    let mut order: Vec<usize> = Vec::new();
    let mut stack = vec![(root.op, false)];
    while let Some((at, ready)) = stack.pop() {
        if out.step_of.get(at).copied().flatten().is_some() {
            continue;
        }
        let Some(op) = def.ops.get(at) else { continue };
        if ready {
            if let Some(s) = out.step_of.get_mut(at) {
                *s = Some(order.len());
            }
            order.push(at);
            continue;
        }
        stack.push((at, true));
        for i in op.inputs.iter().rev() {
            if out.step_of.get(i.op).copied().flatten().is_none() {
                stack.push((i.op, false));
            }
        }
    }
    let input_names = |inputs: &[shards_dockerfile::llb::Input], step_of: &[Option<usize>]| -> Vec<Json> {
        inputs
            .iter()
            .map(|i| {
                let n = step_of.get(i.op).copied().flatten().unwrap_or(0);
                Json::s(format!("step{n}:{}", i.index))
            })
            .collect()
    };
    for (n, &at) in order.iter().enumerate() {
        let Some(op) = def.ops.get(at) else { continue };
        let mut step = vec![("id", Json::s(format!("step{n}")))];
        step.push(("op", op_json(op)));
        if !op.inputs.is_empty() {
            step.push(("inputs", Json::Arr(input_names(&op.inputs, &out.step_of))));
        }
        out.steps.push(Json::obj(step));
        if let Some(d) = digests.get(at) {
            out.mapping
                .push((String::from_utf8_lossy(d).into_owned(), format!("step{n}")));
        }
    }
    // The root: an op of no kind, its one input the result.
    let n = order.len();
    let root_digest = digests
        .get(root.op)
        .map(|d| sha256(&shards_dockerfile::pb::root(d, root.index)))
        .unwrap_or_default();
    let root_step = out.step_of.get(root.op).copied().flatten().unwrap_or(0);
    out.steps.push(Json::obj(vec![
        ("id", Json::s(format!("step{n}"))),
        ("op", Json::obj(vec![("Op", Json::Obj(Vec::new()))])),
        (
            "inputs",
            Json::Arr(vec![Json::s(format!("step{root_step}:{}", root.index))]),
        ),
    ]));
    out.mapping.push((root_digest, format!("step{n}")));
    out
}

/// `json.Marshal` of a Go map's keys: every object's fields in key order, as BuildKit's
/// internal parameters are written (their MarshalJSON goes through a map).
pub fn sorted(j: Json) -> Json {
    match j {
        Json::Obj(mut fields) => {
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            Json::Obj(fields.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Json::Arr(items) => Json::Arr(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// `buildConfig`: the steps and the digest mapping, keys in order.
pub fn build_config(s: &Steps) -> Json {
    let mut mapping = s.mapping.clone();
    mapping.sort();
    sorted(Json::obj(vec![
        ("llbDefinition", Json::Arr(s.steps.clone())),
        (
            "digestMapping",
            Json::Obj(mapping.into_iter().map(|(d, n)| (d, Json::s(n))).collect()),
        ),
    ]))
}

/// The Dockerfile's own definition, as dockerui loads it (`load build definition from`):
/// a local source of the Dockerfile and its `.dockerignore` (and `dockerfile`, Docker's
/// other casing, beside a `Dockerfile`), for a session of the build's.
pub fn dockerfile_definition(
    filename: &str,
    local: &str,
    session: &str,
) -> shards_dockerfile::llb::Definition {
    use shards_dockerfile::llb::{Definition, Input, Meta, Op, OpKind};
    let mut paths = vec![filename.to_string(), format!("{filename}.dockerignore")];
    let (dir, base) = filename.rsplit_once('/').unwrap_or(("", filename));
    if base == "Dockerfile" {
        paths.push(if dir.is_empty() {
            "dockerfile".to_string()
        } else {
            format!("{dir}/dockerfile")
        });
    }
    let follow = Json::Arr(paths.into_iter().map(Json::s).collect()).compact();
    let mut attrs = BTreeMap::new();
    attrs.insert(b"local.differ".to_vec(), b"none".to_vec());
    attrs.insert(b"local.followpaths".to_vec(), follow.into_bytes());
    attrs.insert(b"local.session".to_vec(), session.as_bytes().to_vec());
    attrs.insert(b"local.sharedkeyhint".to_vec(), local.as_bytes().to_vec());
    Definition {
        ops: vec![Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: format!("local://{local}").into_bytes(),
                attrs,
            },
            platform: None,
        }],
        metadata: vec![Meta::default()],
        root: Some(Input { op: 0, index: 0 }),
    }
}

/// `buildkit_metadata.source`: each step's locations in the Dockerfile (its lines, the
/// source map's first source), and the Dockerfile itself with its own definition's steps.
pub fn source(
    def: &shards_dockerfile::llb::Definition,
    s: &Steps,
    info: (&str, &[u8]),
    info_def: &Steps,
) -> Json {
    use base64::Engine as _;
    let mut locations: Vec<(String, Json)> = Vec::new();
    for (at, md) in def.metadata.iter().enumerate() {
        let Some(n) = s.step_of.get(at).copied().flatten() else {
            continue;
        };
        let locs: Vec<Json> = md
            .locations
            .iter()
            .map(|l| {
                let ranges = l
                    .iter()
                    .map(|&(a, b)| {
                        let line =
                            |l: usize| Json::obj(vec![("line", Json::Int(i64::try_from(l).unwrap_or(0)))]);
                        Json::obj(vec![("start", line(a)), ("end", line(b))])
                    })
                    .collect();
                Json::obj(vec![("ranges", Json::Arr(ranges))])
            })
            .collect();
        let entry = if locs.is_empty() {
            Json::Obj(Vec::new())
        } else {
            Json::obj(vec![("locations", Json::Arr(locs))])
        };
        locations.push((format!("step{n}"), entry));
    }
    locations.sort_by(|a, b| a.0.cmp(&b.0));
    let mut mapping = info_def.mapping.clone();
    mapping.sort();
    let (filename, data) = info;
    let info = Json::obj(vec![
        ("filename", Json::s(filename)),
        ("language", Json::s("Dockerfile")),
        (
            "data",
            Json::s(base64::engine::general_purpose::STANDARD.encode(data)),
        ),
        (
            "llbDefinition",
            Json::Arr(info_def.steps.iter().cloned().map(declared).collect()),
        ),
        (
            "digestMapping",
            Json::Obj(mapping.into_iter().map(|(d, n)| (d, Json::s(n))).collect()),
        ),
    ]);
    Json::obj(vec![
        ("locations", Json::Obj(locations)),
        ("infos", Json::Arr(vec![info])),
    ])
}

/// A step as Go writes `BuildStep` where no map reorders it: its ID, op and inputs.
fn declared(step: Json) -> Json {
    let Json::Obj(fields) = step else { return step };
    let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut out = Vec::new();
    for k in ["id", "op", "inputs"] {
        if let Some(v) = get(k) {
            out.push((k.to_string(), v));
        }
    }
    Json::Obj(out)
}

/// `pb.Op` as Go's encoding/json writes it (solver/pb/json.go): what it does under `Op`,
/// its platform and its constraints, every field left out at its zero value; its inputs
/// left out, as the steps name them. Fields in the structs' order.
pub fn op_json(op: &shards_dockerfile::llb::Op) -> Json {
    use shards_dockerfile::llb::{NetMode, OpKind, Security};
    let kind = match &op.kind {
        OpKind::Exec {
            process,
            mounts,
            network,
            security,
            secret_env,
            devices,
        } => {
            let mut e = vec![("meta", meta_json(process))];
            if !mounts.is_empty() {
                e.push(("mounts", Json::Arr(mounts.iter().map(mount_json).collect())));
            }
            match network {
                NetMode::Sandbox => {}
                NetMode::Host => e.push(("network", Json::Int(1))),
                NetMode::None => e.push(("network", Json::Int(2))),
            }
            if *security == Security::Insecure {
                e.push(("security", Json::Int(1)));
            }
            if !secret_env.is_empty() {
                let se = secret_env
                    .iter()
                    .map(|(id, name, optional)| {
                        let mut f = Vec::new();
                        put_str(&mut f, "ID", id);
                        put_str(&mut f, "name", name);
                        put_bool(&mut f, "optional", *optional);
                        Json::obj(f)
                    })
                    .collect();
                e.push(("secretenv", Json::Arr(se)));
            }
            if !devices.is_empty() {
                let dv = devices
                    .iter()
                    .map(|d| {
                        let mut f = Vec::new();
                        put_str(&mut f, "name", &d.name);
                        put_bool(&mut f, "optional", d.optional);
                        Json::obj(f)
                    })
                    .collect();
                e.push(("cdiDevices", Json::Arr(dv)));
            }
            vec![("exec", Json::obj(e))]
        }
        OpKind::Source { identifier, attrs } => {
            let mut s = Vec::new();
            put_str(&mut s, "identifier", identifier);
            let shown: Vec<(String, Json)> = attrs
                .iter()
                .filter(|(k, _)| k.as_slice() != b"local.session" && k.as_slice() != b"local.unique")
                .map(|(k, v)| {
                    (
                        String::from_utf8_lossy(k).into_owned(),
                        Json::s(String::from_utf8_lossy(v)),
                    )
                })
                .collect();
            if !shown.is_empty() {
                s.push(("attrs", Json::Obj(shown)));
            }
            vec![("source", Json::obj(s))]
        }
        OpKind::File { actions } => {
            let acts = actions.iter().map(action_json).collect();
            vec![("file", Json::obj(vec![("actions", Json::Arr(acts))]))]
        }
        OpKind::Merge => {
            let ins = (0..op.inputs.len())
                .map(|i| {
                    let mut f = Vec::new();
                    put_int(&mut f, "input", i64::try_from(i).unwrap_or(0));
                    Json::obj(f)
                })
                .collect();
            vec![("merge", Json::obj(vec![("inputs", Json::Arr(ins))]))]
        }
        // shards' own step (D54), which BuildKit has no op for: under its own name.
        OpKind::Skills { name } => {
            vec![(
                "skills",
                Json::obj(vec![("name", Json::s(String::from_utf8_lossy(name)))]),
            )]
        }
    };
    let mut out = vec![("Op", Json::obj(kind))];
    if let Some(p) = &op.platform {
        let mut f = Vec::new();
        put_str(&mut f, "Architecture", &p.architecture);
        put_str(&mut f, "OS", &p.os);
        put_str(&mut f, "Variant", &p.variant);
        put_str(&mut f, "OSVersion", &p.os_version);
        if !p.os_features.is_empty() {
            f.push((
                "OSFeatures",
                Json::Arr(
                    p.os_features
                        .iter()
                        .map(|x| Json::s(String::from_utf8_lossy(x)))
                        .collect(),
                ),
            ));
        }
        out.push(("platform", Json::obj(f)));
    }
    out.push(("constraints", Json::Obj(Vec::new())));
    Json::obj(out)
}

fn put_str(f: &mut Vec<(&'static str, Json)>, k: &'static str, v: &[u8]) {
    if !v.is_empty() {
        f.push((k, Json::s(String::from_utf8_lossy(v))));
    }
}

fn put_bool(f: &mut Vec<(&'static str, Json)>, k: &'static str, v: bool) {
    if v {
        f.push((k, Json::Bool(true)));
    }
}

fn put_int(f: &mut Vec<(&'static str, Json)>, k: &'static str, v: i64) {
    if v != 0 {
        f.push((k, Json::Int(v)));
    }
}

fn strs(v: &[Vec<u8>]) -> Json {
    Json::Arr(v.iter().map(|s| Json::s(String::from_utf8_lossy(s))).collect())
}

fn meta_json(p: &shards_dockerfile::llb::Process) -> Json {
    let mut m = Vec::new();
    if !p.args.is_empty() {
        m.push(("args", strs(&p.args)));
    }
    if !p.env.is_empty() {
        m.push(("env", strs(&p.env)));
    }
    put_str(&mut m, "cwd", &p.cwd);
    put_str(&mut m, "user", &p.user);
    if let Some(x) = &p.proxy {
        let mut f = Vec::new();
        put_str(&mut f, "http_proxy", &x.http);
        put_str(&mut f, "https_proxy", &x.https);
        put_str(&mut f, "ftp_proxy", &x.ftp);
        put_str(&mut f, "no_proxy", &x.no);
        put_str(&mut f, "all_proxy", &x.all);
        m.push(("proxy_env", Json::obj(f)));
    }
    if !p.extra_hosts.is_empty() {
        let hosts = p
            .extra_hosts
            .iter()
            .map(|h| {
                let mut f = Vec::new();
                put_str(&mut f, "Host", &h.host);
                put_str(&mut f, "IP", &h.ip);
                Json::obj(f)
            })
            .collect();
        m.push(("extraHosts", Json::Arr(hosts)));
    }
    put_str(&mut m, "hostname", &p.hostname);
    if !p.ulimits.is_empty() {
        let us = p
            .ulimits
            .iter()
            .map(|u| {
                let mut f = Vec::new();
                put_str(&mut f, "Name", &u.name);
                put_int(&mut f, "Soft", u.soft);
                put_int(&mut f, "Hard", u.hard);
                Json::obj(f)
            })
            .collect();
        m.push(("ulimit", Json::Arr(us)));
    }
    put_str(&mut m, "cgroupParent", &p.cgroup_parent);
    m.push(("removeMountStubsRecursive", Json::Bool(true)));
    Json::obj(m)
}

fn mount_json(mt: &shards_dockerfile::llb::OpMount) -> Json {
    use shards_dockerfile::llb::{OpMountKind, Sharing};
    let mut x = Vec::new();
    put_int(&mut x, "input", mt.input);
    put_str(&mut x, "selector", &mt.selector);
    put_str(&mut x, "dest", &mt.dest);
    put_int(&mut x, "output", mt.output);
    put_bool(&mut x, "readonly", mt.readonly);
    match &mt.kind {
        OpMountKind::Bind => {}
        OpMountKind::Tmpfs { size } => {
            x.push(("mountType", Json::Int(4)));
            let mut t = Vec::new();
            put_int(&mut t, "size", *size);
            x.push(("TmpfsOpt", Json::obj(t)));
        }
        OpMountKind::Cache { id, sharing } => {
            x.push(("mountType", Json::Int(3)));
            let mut c = Vec::new();
            put_str(&mut c, "ID", id);
            match sharing {
                Sharing::Shared => {}
                Sharing::Private => c.push(("sharing", Json::Int(1))),
                Sharing::Locked => c.push(("sharing", Json::Int(2))),
            }
            x.push(("cacheOpt", Json::obj(c)));
        }
        OpMountKind::Secret {
            id,
            uid,
            gid,
            mode,
            optional,
        }
        | OpMountKind::Ssh {
            id,
            uid,
            gid,
            mode,
            optional,
        } => {
            let secret = matches!(mt.kind, OpMountKind::Secret { .. });
            x.push(("mountType", Json::Int(if secret { 1 } else { 2 })));
            let mut s = Vec::new();
            put_str(&mut s, "ID", id);
            put_int(&mut s, "uid", i64::from(*uid));
            put_int(&mut s, "gid", i64::from(*gid));
            put_int(&mut s, "mode", i64::from(*mode));
            put_bool(&mut s, "optional", *optional);
            x.push((if secret { "secretOpt" } else { "SSHOpt" }, Json::obj(s)));
        }
    }
    Json::obj(x)
}

fn chown_json(o: &shards_dockerfile::llb::OpChown) -> Json {
    use shards_dockerfile::llb::OpUser;
    let user = |u: &OpUser| {
        let inner = match u {
            OpUser::Name { name, input } => {
                let mut n = Vec::new();
                put_str(&mut n, "name", name);
                put_int(&mut n, "input", *input);
                vec![("byName", Json::obj(n))]
            }
            OpUser::Id(id) => {
                let mut f = Vec::new();
                put_int(&mut f, "byId", i64::from(*id));
                f
            }
        };
        Json::obj(vec![("User", Json::obj(inner))])
    };
    let mut f = Vec::new();
    if let Some(u) = &o.user {
        f.push(("user", user(u)));
    }
    if let Some(g) = &o.group {
        f.push(("group", user(g)));
    }
    Json::obj(f)
}

fn action_json(a: &shards_dockerfile::llb::OpAction) -> Json {
    use base64::Engine as _;
    use shards_dockerfile::llb::OpActionKind;
    let (key, body) = match &a.action {
        OpActionKind::Copy {
            src,
            dest,
            owner,
            mode,
            mode_str,
            follow_symlink,
            dir_copy_contents,
            attempt_unpack,
            create_dest_path,
            allow_wildcard,
            allow_empty_wildcard,
            timestamp,
            include_patterns,
            exclude_patterns,
            required_paths,
        } => {
            let mut c = Vec::new();
            put_str(&mut c, "src", src);
            put_str(&mut c, "dest", dest);
            if let Some(o) = owner {
                c.push(("owner", chown_json(o)));
            }
            put_int(&mut c, "mode", i64::from(*mode));
            put_bool(&mut c, "followSymlink", *follow_symlink);
            put_bool(&mut c, "dirCopyContents", *dir_copy_contents);
            put_bool(&mut c, "attemptUnpackDockerCompatibility", *attempt_unpack);
            put_bool(&mut c, "createDestPath", *create_dest_path);
            put_bool(&mut c, "allowWildcard", *allow_wildcard);
            put_bool(&mut c, "allowEmptyWildcard", *allow_empty_wildcard);
            put_int(&mut c, "timestamp", *timestamp);
            if !include_patterns.is_empty() {
                c.push(("include_patterns", strs(include_patterns)));
            }
            if !exclude_patterns.is_empty() {
                c.push(("exclude_patterns", strs(exclude_patterns)));
            }
            put_str(&mut c, "modeStr", mode_str);
            if !required_paths.is_empty() {
                c.push(("required_paths", strs(required_paths)));
            }
            ("copy", c)
        }
        OpActionKind::Mkfile {
            path,
            mode,
            data,
            owner,
            timestamp,
        } => {
            let mut f = Vec::new();
            put_str(&mut f, "path", path);
            put_int(&mut f, "mode", i64::from(*mode));
            if !data.is_empty() {
                f.push((
                    "data",
                    Json::s(base64::engine::general_purpose::STANDARD.encode(data)),
                ));
            }
            if let Some(o) = owner {
                f.push(("owner", chown_json(o)));
            }
            put_int(&mut f, "timestamp", *timestamp);
            ("mkfile", f)
        }
        OpActionKind::Mkdir {
            path,
            mode,
            make_parents,
            owner,
            timestamp,
        } => {
            let mut f = Vec::new();
            put_str(&mut f, "path", path);
            put_int(&mut f, "mode", i64::from(*mode));
            put_bool(&mut f, "makeParents", *make_parents);
            if let Some(o) = owner {
                f.push(("owner", chown_json(o)));
            }
            put_int(&mut f, "timestamp", *timestamp);
            ("mkdir", f)
        }
    };
    // jsonFileAction: its indexes always, then the action.
    Json::obj(vec![
        ("input", Json::Int(a.input)),
        ("secondaryInput", Json::Int(a.secondary_input)),
        ("output", Json::Int(a.output)),
        ("Action", Json::obj(vec![(key, Json::obj(body))])),
    ])
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
        /// Its SBOMs' statements, as the image holds them (D81).
        #[serde(default)]
        sboms: Vec<String>,
        /// A local output's `sbom.spdx.json`.
        #[serde(default)]
        localsbom: Option<String>,
    }

    /// A recorded SBOM statement as its scanner's file: named for its document, as
    /// buildkit-syft-scanner names each scan.
    fn scanned(text: &str) -> super::super::sbom::Scanned {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        let path = format!("{}.spdx.json", v["predicate"]["name"].as_str().unwrap());
        super::super::sbom::statement(&path, text.as_bytes()).unwrap()
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
                // A layer, as busybox has: an image of none is planned as scratch.
                config: br#"{"architecture":"arm64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":["sha256:0000000000000000000000000000000000000000000000000000000000000001"]}}"#.to_vec(),
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
            // As buildx asks Docker's builder (buildx_attrs).
            image_resolve_mode: b"local".to_vec(),
            main_context: Default::default(),
            context_subdir: None,
            ..Default::default()
        };
        shards_dockerfile::plan::plan(dockerfile.as_bytes(), &opts, &AnyImage)
            .unwrap()
            .definition()
    }

    /// `s` with each `"digestMapping":{...}` as its steps, sorted: what a digest maps to,
    /// not the digests, which cover a session's random IDs.
    fn steps_only(s: &str) -> String {
        let key = "\"digestMapping\":{";
        let mut out = String::new();
        let mut rest = s;
        while let Some((before, after)) = rest.split_once(key) {
            out.push_str(before);
            out.push_str(key);
            let (body, tail) = after.split_once('}').unwrap();
            let mut steps: Vec<&str> = body
                .split(',')
                .filter_map(|kv| kv.rsplit_once(':').map(|(_, v)| v))
                .collect();
            steps.sort_unstable();
            out.push_str(&steps.join(","));
            out.push('}');
            rest = tail;
        }
        out.push_str(rest);
        out
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
        // A local output's SBOM: the statement, indented, its files its subjects (D81).
        let local_sbom = all.iter().find(|c| c.name == "sbom-local").unwrap();
        let text = local_sbom.localsbom.as_ref().unwrap();
        let recorded: serde_json::Value = serde_json::from_str(text).unwrap();
        let files: Vec<(String, String)> = recorded["subject"]
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
        // As its scanner wrote it: the statement with no subjects of its own.
        let mut bare = recorded.clone();
        bare["subject"] = serde_json::json!([]);
        let s = super::super::sbom::statement("sbom.spdx.json", bare.to_string().as_bytes()).unwrap();
        assert_eq!(
            String::from_utf8(shards_dockerfile::json_indent(
                super::super::sbom::intoto(&s, &files).as_bytes()
            ))
            .unwrap(),
            *text
        );
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
                max: None,
                sboms: Vec::new(),
            };
            // Its SBOMs (D81), the core target's first, then by name.
            let mut sboms: Vec<super::super::sbom::Scanned> = case.sboms.iter().map(|t| scanned(t)).collect();
            sboms.sort_by(|a, b| {
                let core = |s: &super::super::sbom::Scanned| s.path.split('.').next() != Some("sbom");
                (core(a), &a.path).cmp(&(core(b), &b.path))
            });
            capture.sboms = sboms;
            // mode=max (D80): the plan's steps and source map; the layers are a real
            // build's (`provenance_max_records_a_builds_steps_and_layers`), so the recording's.
            if case.flags.iter().any(|f| f == "--provenance=mode=max") {
                let dockerfile = case.dockerfile.as_ref().unwrap();
                let def = plan_of(dockerfile);
                let s = steps(&def);
                let info = steps(&dockerfile_definition("Dockerfile", "dockerfile", "session"));
                let md = &recorded["predicate"]["runDetails"]["metadata"]["buildkit_metadata"];
                let layers = md["layers"]
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, chains)| {
                                let chains = chains
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .map(|c| {
                                        Json::Arr(
                                            c.as_array()
                                                .unwrap()
                                                .iter()
                                                .map(|d| {
                                                    layer_descriptor(
                                                        d["mediaType"].as_str().unwrap(),
                                                        d["digest"].as_str().unwrap(),
                                                        d["size"].as_u64().unwrap(),
                                                    )
                                                })
                                                .collect(),
                                        )
                                    })
                                    .collect();
                                (k.clone(), Json::Arr(chains))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                capture.max = Some(Max {
                    build_config: build_config(&s),
                    source: source(&def, &s, ("Dockerfile", dockerfile.as_bytes()), &info),
                    layers,
                });
            }
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
            // Ops' digests cover a session's IDs: each digest mapping as its steps alone.
            assert_eq!(steps_only(&ours), steps_only(st), "{}", case.name);
            assert_eq!(ours.len(), st.len(), "{}", case.name);
            // The attestation: its SBOMs, as the image holds them, then its provenance.
            // The provenance as recorded: compared above but for its digests, which a
            // session's IDs make (mode=max).
            let mut statements = image_statements(&capture, &run, &subjects, false);
            statements.push((st.clone(), PREDICATE_TYPE.to_string()));
            // Each SBOM statement byte for byte as the image holds it; their order the
            // manifest's, below.
            for (text, kind) in &statements {
                if kind == super::super::sbom::PREDICATE {
                    assert!(case.sboms.contains(text), "{}: {text}", case.name);
                }
            }
            let digests: Vec<String> = statements.iter().map(|(t, _)| sha256(t.as_bytes())).collect();
            let listed: Vec<(&str, usize, &str)> = statements
                .iter()
                .zip(&digests)
                .map(|((t, kind), d)| (d.as_str(), t.len(), kind.as_str()))
                .collect();
            let (config, manifest) = attestation_of(&listed);
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
