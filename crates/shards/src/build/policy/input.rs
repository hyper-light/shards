//! What a policy is asked about, as buildx v0.37.1 tells it (policy/types.go,
//! policy/validate.go sourceToInput): a source, in Go's JSON (fields in their declared
//! order, the empty ones left out), and the parts of it not known yet, which the
//! source's metadata answers.

use std::collections::{BTreeMap, BTreeSet};

use shards_dockerfile::json::Value as JsonValue;
use shards_dockerfile::platform::{self, Platform};
use shards_image::reference::Reference;

use shards_cmdline::buildflags::LogLevel;

use super::gitobject::{self, Actor, Commit, Tag};
use super::provenance::{self, Provenance};
use super::signatures::{self, Trust};
use super::{Meta, Source};

/// A JSON document as encoding/json writes Go's values: an object's fields in the order
/// written, a map's in key order.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// `json.MarshalIndent(v, "", "  ")`.
    pub fn indented(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(0));
        out
    }

    /// `json.Marshal(v)`.
    #[cfg(test)]
    pub fn compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None);
        out
    }

    /// The document at `depth`, indented, or compact where `None`.
    fn write(&self, out: &mut String, depth: Option<usize>) {
        let pad = |out: &mut String, d: Option<usize>| {
            if let Some(d) = d {
                out.push('\n');
                for _ in 0..d {
                    out.push_str("  ");
                }
            }
        };
        let inner = depth.map(|d| d + 1);
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Str(s) => go_string(out, s),
            Json::Arr(a) if a.is_empty() => out.push_str("[]"),
            Json::Obj(o) if o.is_empty() => out.push_str("{}"),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, inner);
                    v.write(out, inner);
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, inner);
                    go_string(out, k);
                    out.push_str(if depth.is_some() { ": " } else { ":" });
                    v.write(out, inner);
                }
                pad(out, depth);
                out.push('}');
            }
        }
    }

    /// The document as a Rego value, as ast.InterfaceToValue makes one of it.
    pub fn value(&self) -> shards_rego::value::Value {
        use shards_rego::value::Value;
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Int(i) => Value::int(*i),
            Json::Str(s) => Value::string(s.as_str()),
            Json::Arr(a) => Value::array(a.iter().map(Json::value).collect()),
            Json::Obj(o) => Value::object(
                o.iter()
                    .map(|(k, v)| (Value::string(k.as_str()), v.value()))
                    .collect(),
            ),
        }
    }
}

/// encoding/json's string: `<`, `>`, `&`, U+2028 and U+2029 escaped, as are control
/// characters, `\b`, `\f`, `\n`, `\r` and `\t` by name.
fn go_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A struct's fields as encoding/json writes them with `omitempty`: those not empty, in
/// order.
#[derive(Default)]
pub(super) struct Fields(Vec<(String, Json)>);

impl Fields {
    pub(super) fn str(mut self, k: &str, v: &str) -> Self {
        if !v.is_empty() {
            self.0.push((k.into(), Json::Str(v.into())));
        }
        self
    }

    pub(super) fn flag(mut self, k: &str, v: bool) -> Self {
        if v {
            self.0.push((k.into(), Json::Bool(true)));
        }
        self
    }

    fn strs(mut self, k: &str, v: &[String]) -> Self {
        if !v.is_empty() {
            self.0.push((
                k.into(),
                Json::Arr(v.iter().map(|s| Json::Str(s.clone())).collect()),
            ));
        }
        self
    }

    pub(super) fn map(mut self, k: &str, v: &BTreeMap<String, String>) -> Self {
        if !v.is_empty() {
            let o = v.iter().map(|(a, b)| (a.clone(), Json::Str(b.clone()))).collect();
            self.0.push((k.into(), Json::Obj(o)));
        }
        self
    }

    pub(super) fn json(mut self, k: &str, v: Option<Json>) -> Self {
        if let Some(v) = v {
            self.0.push((k.into(), v));
        }
        self
    }

    pub(super) fn done(self) -> Json {
        Json::Obj(self.0)
    }
}

/// `Env`: what the build says of itself.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// Each build argument, `None` for one given without a value.
    pub args: BTreeMap<String, Option<String>>,
    pub labels: BTreeMap<String, String>,
    pub filename: String,
    pub target: String,
    pub caps_request: bool,
    pub depth: i64,
}

impl Env {
    /// Whether `omitzero` leaves it out: nothing set, at depth 0.
    fn is_zero(&self) -> bool {
        self.args.is_empty()
            && self.labels.is_empty()
            && self.filename.is_empty()
            && self.target.is_empty()
            && !self.caps_request
            && self.depth == 0
    }

    fn json(&self) -> Json {
        let mut f = Fields::default();
        if !self.args.is_empty() {
            let args = self
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.as_ref().map_or(Json::Null, |v| Json::Str(v.clone()))))
                .collect();
            f.0.push(("args".into(), Json::Obj(args)));
        }
        let mut f = f
            .map("labels", &self.labels)
            .str("filename", &self.filename)
            .str("target", &self.target)
            .flag("capsRequest", self.caps_request);
        // `depth` has no omitempty.
        f.0.push(("depth".into(), Json::Int(self.depth)));
        f.done()
    }
}

/// `Image`: a `docker-image://` source.
#[derive(Debug, Clone, Default)]
pub struct Image {
    pub reference: String,
    pub host: String,
    pub repo: String,
    pub full_repo: String,
    pub tag: String,
    pub platform: String,
    pub os: String,
    pub arch: String,
    pub variant: String,
    pub is_canonical: bool,
    pub checksum: String,
    pub created: String,
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub user: String,
    pub volumes: Vec<String>,
    pub working_dir: String,
    /// Whether the image has an attestation manifest or a provenance.
    pub has_provenance: bool,
    pub provenance: Option<Box<Provenance>>,
    /// D106 SIGNATURES HOOK: `signatures` ([]AttestationSignature, parseSignatures over
    /// the attestation chain), each as its JSON. `None` until that is in; written after
    /// `provenance`, and only when it holds any (omitempty).
    pub signatures: Option<Vec<Json>>,
    /// An OSI artifact's type (D116), written last and only for one: an image has none.
    pub artifact_type: String,
    /// An OSI artifact's signature bundles, each its JSON, for `verify_image_signature`;
    /// never written.
    pub bundles: Option<Vec<Vec<u8>>>,
}

impl Image {
    pub fn json(&self) -> Json {
        Fields::default()
            .str("ref", &self.reference)
            .str("host", &self.host)
            .str("repo", &self.repo)
            .str("fullRepo", &self.full_repo)
            .str("tag", &self.tag)
            .str("platform", &self.platform)
            .str("os", &self.os)
            .str("arch", &self.arch)
            .str("variant", &self.variant)
            .flag("isCanonical", self.is_canonical)
            .str("checksum", &self.checksum)
            .str("createdTime", &self.created)
            .strs("env", &self.env)
            .map("labels", &self.labels)
            .str("user", &self.user)
            .strs("volumes", &self.volumes)
            .str("workingDir", &self.working_dir)
            .flag("hasProvenance", self.has_provenance)
            .json("provenance", self.provenance.as_ref().map(|p| p.json()))
            .json(
                "signatures",
                self.signatures
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|s| Json::Arr(s.clone())),
            )
            .str("artifactType", &self.artifact_type)
            .done()
    }
}

/// `HTTP`: an `http://` or `https://` source.
#[derive(Debug, Clone, Default)]
pub struct Http {
    pub url: String,
    pub schema: String,
    pub host: String,
    pub path: String,
    pub query: BTreeMap<String, Vec<String>>,
    pub has_auth: bool,
    pub checksum: String,
    /// The digest of the content and a signature's hash suffix, and that suffix, where
    /// `verify_http_pgp_signature` asked for them (checksumResponseForSignature).
    pub signature_checksum: Option<(String, Vec<u8>)>,
}

impl Http {
    pub fn json(&self) -> Json {
        let mut f = Fields::default()
            .str("url", &self.url)
            .str("schema", &self.schema)
            .str("host", &self.host)
            .str("path", &self.path);
        if !self.query.is_empty() {
            let q = self
                .query
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Json::Arr(v.iter().map(|x| Json::Str(x.clone())).collect()),
                    )
                })
                .collect();
            f.0.push(("query".into(), Json::Obj(q)));
        }
        f.flag("hasAuth", self.has_auth)
            .str("checksum", &self.checksum)
            .done()
    }
}

/// `Actor`, `omitzero` where empty.
fn actor_json(a: &Actor) -> Option<Json> {
    let j = Fields::default()
        .str("name", &a.name)
        .str("email", &a.email)
        .str("when", a.when.as_deref().unwrap_or_default())
        .done();
    (!matches!(&j, Json::Obj(o) if o.is_empty())).then_some(j)
}

/// A Git object's signature, as buildx summarizes it.
fn signature_json(f: Fields, s: &shards_gitsign::Summary) -> Fields {
    match s {
        shards_gitsign::Summary::None => f,
        shards_gitsign::Summary::Pgp { version, key_id } => {
            let mut sig = Fields::default();
            if *version != 0 {
                sig.0.push(("version".into(), Json::Int(i64::from(*version))));
            }
            let sig = sig.str("keyID", &key_id.map(|k| format!("{k:016x}")).unwrap_or_default());
            f.json("pgpSignature", Some(sig.done()))
        }
        shards_gitsign::Summary::Ssh { version, fingerprint } => {
            let mut sig = Fields::default();
            if *version != 0 {
                sig.0.push(("version".into(), Json::Int(i64::from(*version))));
            }
            let sig = sig.str("pubKey", fingerprint);
            f.json("sshSignature", Some(sig.done()))
        }
    }
}

/// `Commit`, and the object it was read from, which `verify_git_signature` verifies.
#[derive(Debug, Clone)]
pub struct CommitInput {
    pub commit: Commit,
    pub signature: shards_gitsign::Summary,
    pub object: gitobject::GitObject,
}

/// `Tag`, and the object it was read from.
#[derive(Debug, Clone)]
pub struct TagInput {
    pub tag: Tag,
    pub signature: shards_gitsign::Summary,
    pub object: gitobject::GitObject,
}

/// `Git`: a `git://` source.
#[derive(Debug, Clone, Default)]
pub struct Git {
    pub schema: String,
    pub host: String,
    pub remote: String,
    pub full_url: String,
    pub tag_name: String,
    pub branch: String,
    pub reference: String,
    pub subdir: String,
    pub is_commit_ref: bool,
    pub is_sha256: bool,
    pub checksum: String,
    pub commit_checksum: String,
    pub is_annotated_tag: bool,
    pub tag: Option<TagInput>,
    pub commit: Option<CommitInput>,
}

impl Git {
    pub fn json(&self) -> Json {
        let tag = self.tag.as_ref().map(|t| {
            let f = Fields::default()
                .str("object", &t.tag.object)
                .str("type", &t.tag.kind)
                .str("tag", &t.tag.tag)
                .json("tagger", actor_json(&t.tag.tagger))
                .str("message", &t.tag.message);
            signature_json(f, &t.signature).done()
        });
        let commit = self.commit.as_ref().map(|c| {
            let f = Fields::default()
                .str("tree", &c.commit.tree)
                .strs("parents", &c.commit.parents)
                .json("author", actor_json(&c.commit.author))
                .json("committer", actor_json(&c.commit.committer))
                .str("message", &c.commit.message);
            signature_json(f, &c.signature).done()
        });
        Fields::default()
            .str("schema", &self.schema)
            .str("host", &self.host)
            .str("remote", &self.remote)
            .str("fullURL", &self.full_url)
            .str("tagName", &self.tag_name)
            .str("branch", &self.branch)
            .str("ref", &self.reference)
            .str("subDir", &self.subdir)
            .flag("isCommitRef", self.is_commit_ref)
            .flag("isSHA256", self.is_sha256)
            .str("checksum", &self.checksum)
            .str("commitChecksum", &self.commit_checksum)
            .flag("isAnnotatedTag", self.is_annotated_tag)
            .json("tag", tag)
            .json("commit", commit)
            .done()
    }
}

/// `Input`: the source, and the build's own `Env` (`omitzero`).
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub env: Env,
    pub local: Option<String>,
    pub image: Option<Image>,
    pub http: Option<Http>,
    pub git: Option<Git>,
    /// The fields not known yet, as `input.`-less refs (`image.checksum`).
    pub unknowns: Vec<String>,
}

impl Input {
    pub fn json(&self) -> Json {
        Fields::default()
            .json("env", (!self.env.is_zero()).then(|| self.env.json()))
            .json(
                "local",
                self.local
                    .as_ref()
                    .map(|name| Fields::default().str("name", name).done()),
            )
            .json("image", self.image.as_ref().map(Image::json))
            .json("http", self.http.as_ref().map(Http::json))
            .json("git", self.git.as_ref().map(Git::json))
            .done()
    }

    /// `Input.Unknowns`: the refs not known yet, each from `input`, then each provenance
    /// material's, from `input.image.provenance.materials[N]` (collectInputUnknowns).
    pub fn unknown_refs(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_unknowns("input", &mut out);
        out
    }

    fn collect_unknowns(&self, prefix: &str, out: &mut Vec<String>) {
        out.extend(
            self.unknowns
                .iter()
                .filter(|u| !u.is_empty())
                .map(|u| format!("{prefix}.{u}")),
        );
        if let Some(p) = self.image.as_ref().and_then(|i| i.provenance.as_ref()) {
            for (i, m) in p.materials.iter().enumerate() {
                m.collect_unknowns(&format!("{prefix}.image.provenance.materials[{i}]"), out);
            }
        }
    }
}

/// The fields of an image's config a policy may ask for.
const CONFIG_FIELDS: [&str; 5] = ["labels", "user", "volumes", "workingDir", "env"];

/// `sourceToInput`: the input for `source` as its metadata `meta` tells it, for
/// `platform`, and what it leaves unknown; what it notes said to `log`.
pub fn of_source(
    source: &Source,
    meta: &Meta,
    wanted: Option<&Platform>,
    trust: Option<&Trust>,
    log: &mut dyn FnMut(LogLevel, &str),
) -> Result<Input, String> {
    let mut inp = Input::default();
    let Some((scheme, rest)) = source.identifier.split_once("://") else {
        return Err(format!("invalid source identifier: {}", source.identifier));
    };
    match scheme {
        "docker-image" => {
            let r = Reference::parse_normalized(rest)
                .map_err(|e| format!("failed to parse image source reference: {e}"))?;
            let mut img = Image {
                reference: r.to_string(),
                host: r.domain.clone(),
                repo: r.familiar_name(),
                full_repo: r.name(),
                ..Image::default()
            };
            if let Some(d) = &r.digest {
                img.checksum = d.to_string();
                img.is_canonical = true;
            }
            if let Some(t) = &r.tag {
                img.tag = t.clone();
            }
            let p = wanted.ok_or("platform required for image source")?;
            img.platform = String::from_utf8_lossy(&platform::format(p)).into_owned();
            img.os = String::from_utf8_lossy(&p.os).into_owned();
            img.arch = String::from_utf8_lossy(&p.architecture).into_owned();
            img.variant = String::from_utf8_lossy(&p.variant).into_owned();
            let config = |f: &str| format!("image.{f}");
            match &meta.image {
                None => {
                    if !img.is_canonical {
                        inp.unknowns.push("image.checksum".into());
                    }
                    inp.unknowns.extend(CONFIG_FIELDS.iter().map(|f| config(f)));
                    inp.unknowns.extend(
                        ["image.hasProvenance", "image.provenance", "image.signatures"].map(String::from),
                    );
                }
                Some(m) => {
                    img.checksum = m.digest.clone();
                    match &m.config {
                        Some(cfg) => config_fields(&mut img, cfg)?,
                        None => inp.unknowns.extend(CONFIG_FIELDS.iter().map(|f| config(f))),
                    }
                    match &m.attestation_chain {
                        Some(chain) => {
                            // Read once a build, where a build's trust keeps readings.
                            let key = trust.map(|_| signatures::chain_key(chain));
                            let read = match (trust, &key) {
                                (Some(t), Some(k)) => t.provenance(k, chain, log),
                                _ => provenance::parse(chain, log),
                            };
                            match read {
                                Ok(p) => img.provenance = p.map(Box::new),
                                Err(e) => {
                                    log(LogLevel::Debug, &format!("failed to parse image provenance: {e}"))
                                }
                            }
                            img.has_provenance =
                                !chain.attestation_manifest.is_empty() || img.provenance.is_some();
                            // parseSignatures, where there is a verifier: a failure is the
                            // debug log's, the field left out. Every input is built for a
                            // platform (CheckPolicy's, or a material's own or its parent's).
                            if let (Some(trust), Some(w), Some(key)) = (trust, wanted, &key) {
                                let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
                                let platform = shards_sigstore::platforms::Platform {
                                    os: text(&w.os),
                                    architecture: text(&w.architecture),
                                    variant: text(&w.variant),
                                    os_version: text(&w.os_version),
                                    os_features: w.os_features.iter().map(|f| text(f)).collect(),
                                };
                                match trust.signatures(key, chain, &platform) {
                                    Ok(sigs) => img.signatures = sigs,
                                    Err(e) => log(
                                        LogLevel::Debug,
                                        &format!("failed to parse image signatures: {e}"),
                                    ),
                                }
                            }
                        }
                        None => inp.unknowns.extend(
                            ["image.hasProvenance", "image.provenance", "image.signatures"].map(String::from),
                        ),
                    }
                }
            }
            inp.image = Some(img);
        }
        "http" | "https" => {
            let u = shards_dockerfile::url::parse(source.identifier.as_bytes())
                .map_err(|e| format!("failed to parse http source url: {}", String::from_utf8_lossy(&e)))?;
            let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            let mut http = Http {
                url: source.identifier.clone(),
                schema: scheme.to_string(),
                host: text(&u.host),
                path: text(&u.path),
                query: u
                    .query()
                    .iter()
                    .map(|(k, v)| (text(k), v.iter().map(|x| text(x)).collect()))
                    .collect(),
                ..Http::default()
            };
            if let Some(m) = &meta.http {
                http.checksum = m.checksum.clone();
                http.signature_checksum = m.signature_checksum.clone();
            }
            if http.checksum.is_empty() {
                inp.unknowns.push("http.checksum".into());
            }
            http.has_auth = source.attrs.contains_key("http.authheadersecret");
            inp.http = Some(http);
        }
        "git" => {
            let (git, unknowns) = git_input(source, rest, meta)?;
            inp.unknowns
                .extend(unknowns.into_iter().map(|u| format!("git.{u}")));
            inp.git = Some(git);
        }
        "local" => inp.local = Some(rest.to_string()),
        // An OSI artifact (D116): to a policy, an object of a registry as an image is,
        // its name's parts, the digest it resolves to, its signatures as an image's, and
        // its artifact type, which no image has.
        "osi-artifact" => {
            let r = Reference::parse_normalized(rest)
                .map_err(|e| format!("failed to parse OSI artifact source reference: {e}"))?;
            let kind = match source.attrs.get("osi.kind").map(String::as_str) {
                Some("agent") | None => shards_image::osi::Kind::Agent,
                Some("harness") => shards_image::osi::Kind::Harness,
                Some("mcp") => shards_image::osi::Kind::Mcp,
                Some(other) => return Err(format!("unsupported OSI artifact kind: {other}")),
            };
            let mut img = Image {
                reference: r.to_string(),
                host: r.domain.clone(),
                repo: r.familiar_name(),
                full_repo: r.name(),
                artifact_type: kind.artifact_type().to_string(),
                ..Image::default()
            };
            if let Some(d) = &r.digest {
                img.checksum = d.to_string();
                img.is_canonical = true;
            }
            if let Some(t) = &r.tag {
                img.tag = t.clone();
            }
            if let Some(p) = wanted {
                img.platform = String::from_utf8_lossy(&platform::format(p)).into_owned();
                img.os = String::from_utf8_lossy(&p.os).into_owned();
                img.arch = String::from_utf8_lossy(&p.architecture).into_owned();
                img.variant = String::from_utf8_lossy(&p.variant).into_owned();
            }
            match &meta.artifact {
                None => {
                    if !img.is_canonical {
                        inp.unknowns.push("image.checksum".into());
                    }
                    inp.unknowns.push("image.signatures".into());
                }
                Some(a) => {
                    // What its referrers refer to: the index of several platforms.
                    img.checksum = a.digest.clone();
                    match (&a.signatures, trust) {
                        (Some(bundles), Some(trust)) => {
                            img.signatures =
                                Some(signatures::artifact_signatures(bundles, &a.digest, trust, log));
                            img.bundles = Some(bundles.clone());
                        }
                        (Some(bundles), None) => img.bundles = Some(bundles.clone()),
                        (None, _) => inp.unknowns.push("image.signatures".into()),
                    }
                }
            }
            inp.image = Some(img);
        }
        _ => return Err(format!("unsupported source scheme: {scheme}")),
    }
    Ok(inp)
}

/// The fields buildx reads of an image's config, as `json.Unmarshal` reads it into an
/// ocispecs.Image (image-spec v1.1.1): when it was made, as RFC 3339 writes it, and what
/// it runs with. Its volumes in name order, where Go's map gives them in no order at all.
fn config_fields(img: &mut Image, raw: &[u8]) -> Result<(), String> {
    let c = OciImage::from_json(raw).map_err(|e| {
        format!(
            "failed to unmarshal image config: {}",
            String::from_utf8_lossy(&e)
        )
    })?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if let Some(t) = &c.created {
        img.created = rfc3339(t);
    }
    img.env = c.env.live().iter().map(|e| text(e)).collect();
    img.labels = c.labels.iter().map(|(k, v)| (text(k), text(v))).collect();
    img.user = text(&c.user);
    img.volumes = c.volumes.iter().map(|v| text(v)).collect();
    img.working_dir = text(&c.working_dir);
    Ok(())
}

/// What buildx reads of an ocispecs.Image, read as encoding/json reads one: a key matched
/// to its field exactly, else as foldName folds both; a repeated key decoding into what an
/// earlier one left, maps and structs merged and a slice's elements reused; `null` making
/// a slice or map nil and leaving a string as it was; every field's type checked, the
/// first mismatch kept while the rest is read; a time parsed from its text as written,
/// its failure ending the reading at once. The document is scanned without recursion
/// (shards_dockerfile::json), so no nesting exhausts a thread's stack.
#[derive(Debug, Default)]
struct OciImage {
    created: Option<shards_dockerfile::go::Time>,
    user: Vec<u8>,
    env: GoStrings,
    volumes: BTreeSet<Vec<u8>>,
    working_dir: Vec<u8>,
    labels: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// A `[]string` as Go's decoder fills one: an array's elements decoded in place over what
/// earlier arrays left in its backing array, a `null` element keeping what was there; a
/// `null` array making it nil and `[]` a new, empty one.
#[derive(Debug, Default)]
struct GoStrings {
    held: Vec<Vec<u8>>,
    len: usize,
}

impl GoStrings {
    fn live(&self) -> &[Vec<u8>] {
        self.held.get(..self.len).unwrap_or_default()
    }
}

/// An `Unmarshaler`'s error (Time.UnmarshalJSON's), which ends the reading at once.
#[derive(Debug)]
struct Abort(Vec<u8>);

/// Where a value goes, for Go's messages: the struct whose field it fills and the path of
/// JSON names (and embedded structs' names) to that field; both empty for the document.
#[derive(Debug, Clone, Copy)]
struct At {
    strukt: &'static str,
    field: &'static str,
}

const DOCUMENT: At = At {
    strukt: "",
    field: "",
};

/// The fields of the structs read, each with its path from the document.
const IMAGE_FIELDS: &[(&str, &str)] = &[
    ("created", "created"),
    ("author", "author"),
    ("architecture", "Platform.architecture"),
    ("os", "Platform.os"),
    ("os.version", "Platform.os.version"),
    ("os.features", "Platform.os.features"),
    ("variant", "Platform.variant"),
    ("config", "config"),
    ("rootfs", "rootfs"),
    ("history", "history"),
];
const CONFIG_FIELDS_OCI: &[(&str, &str)] = &[
    ("User", "config.User"),
    ("ExposedPorts", "config.ExposedPorts"),
    ("Env", "config.Env"),
    ("Entrypoint", "config.Entrypoint"),
    ("Cmd", "config.Cmd"),
    ("Volumes", "config.Volumes"),
    ("WorkingDir", "config.WorkingDir"),
    ("Labels", "config.Labels"),
    ("StopSignal", "config.StopSignal"),
    ("ArgsEscaped", "config.ArgsEscaped"),
];
const ROOTFS_FIELDS: &[(&str, &str)] = &[("type", "rootfs.type"), ("diff_ids", "rootfs.diff_ids")];
const HISTORY_FIELDS: &[(&str, &str)] = &[
    ("created", "history.created"),
    ("created_by", "history.created_by"),
    ("author", "history.author"),
    ("comment", "history.comment"),
    ("empty_layer", "history.empty_layer"),
];

/// foldName of a key: ASCII letters upper-cased, `ſ` and the Kelvin sign folded to `S`
/// and `K`; a key with any other rune that is not ASCII matches no field (they are all
/// ASCII).
fn fold_key(key: &[u8]) -> Option<String> {
    String::from_utf8_lossy(key)
        .chars()
        .map(|c| match c {
            c if c.is_ascii() => Some(c.to_ascii_uppercase()),
            '\u{17F}' => Some('S'),
            '\u{212A}' => Some('K'),
            _ => None,
        })
        .collect()
}

/// The JSON kind Go names in an UnmarshalTypeError.
fn kind(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(..) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

/// A reading: the first type mismatch.
#[derive(Debug, Default)]
struct Reader {
    saved: Option<String>,
}

impl Reader {
    fn mismatch(&mut self, v: &JsonValue, at: At, go_type: &str) {
        if self.saved.is_none() {
            self.saved = Some(shards_sigstore::tlog::gojson::type_error(
                kind(v),
                at.strukt,
                at.field,
                go_type,
            ));
        }
    }

    /// The members of an object that fill the struct `fields` lists, each with where it
    /// goes; none for `null`, and a mismatch for anything else.
    fn members<'v>(
        &mut self,
        v: &'v JsonValue,
        at: At,
        strukt: &'static str,
        go_type: &str,
        fields: &'static [(&'static str, &'static str)],
    ) -> Vec<(&'static str, At, &'v JsonValue)> {
        let JsonValue::Object(members) = v else {
            if !matches!(v, JsonValue::Null) {
                self.mismatch(v, at, go_type);
            }
            return Vec::new();
        };
        members
            .iter()
            .filter_map(|(k, m)| {
                let exact = fields.iter().find(|(name, _)| name.as_bytes() == k.as_slice());
                let found = exact.or_else(|| {
                    let folded = fold_key(k)?;
                    fields
                        .iter()
                        .find(|(name, _)| name.to_ascii_uppercase() == folded)
                })?;
                Some((
                    found.0,
                    At {
                        strukt,
                        field: found.1,
                    },
                    m,
                ))
            })
            .collect()
    }

    fn string(&mut self, v: &JsonValue, at: At, into: &mut Vec<u8>, go_type: &str) {
        match v {
            JsonValue::Null => {}
            JsonValue::String(s, _) => s.clone_into(into),
            _ => self.mismatch(v, at, go_type),
        }
    }

    /// A string field not read: its type checked.
    fn check_string(&mut self, v: &JsonValue, at: At, go_type: &str) {
        if !matches!(v, JsonValue::Null | JsonValue::String(..)) {
            self.mismatch(v, at, go_type);
        }
    }

    fn check_bool(&mut self, v: &JsonValue, at: At) {
        if !matches!(v, JsonValue::Null | JsonValue::Bool(_)) {
            self.mismatch(v, at, "bool");
        }
    }

    /// A slice of a string type not read: its elements' types checked.
    fn check_strings(&mut self, v: &JsonValue, at: At, elem: &str) {
        match v {
            JsonValue::Null => {}
            JsonValue::Array(items) => items.iter().for_each(|x| self.check_string(x, at, elem)),
            _ => self.mismatch(v, at, &format!("[]{elem}")),
        }
    }

    fn strings(&mut self, v: &JsonValue, at: At, into: &mut GoStrings) {
        match v {
            JsonValue::Array(items) if !items.is_empty() => {
                for (i, x) in items.iter().enumerate() {
                    if i >= into.held.len() {
                        into.held.push(Vec::new());
                    }
                    if let Some(slot) = into.held.get_mut(i) {
                        self.string(x, at, slot, "string");
                    }
                }
                into.len = items.len();
            }
            JsonValue::Null | JsonValue::Array(_) => *into = GoStrings::default(),
            _ => self.mismatch(v, at, "[]string"),
        }
    }

    /// A `map[string]struct{}`: keys merged into what is there, each value checked.
    fn set(&mut self, v: &JsonValue, at: At, mut into: Option<&mut BTreeSet<Vec<u8>>>) {
        match v {
            JsonValue::Null => {
                if let Some(s) = into {
                    s.clear();
                }
            }
            JsonValue::Object(members) => {
                for (k, m) in members {
                    if !matches!(m, JsonValue::Null | JsonValue::Object(_)) {
                        self.mismatch(m, at, "struct {}");
                    }
                    if let Some(s) = into.as_deref_mut() {
                        s.insert(k.clone());
                    }
                }
            }
            _ => self.mismatch(v, at, "map[string]struct {}"),
        }
    }

    /// A `map[string]string`: members merged into what is there, `null` as "".
    fn map(&mut self, v: &JsonValue, at: At, into: &mut BTreeMap<Vec<u8>, Vec<u8>>) {
        match v {
            JsonValue::Null => into.clear(),
            JsonValue::Object(members) => {
                for (k, m) in members {
                    let mut s = Vec::new();
                    self.string(m, at, &mut s, "string");
                    into.insert(k.clone(), s);
                }
            }
            _ => self.mismatch(v, at, "map[string]string"),
        }
    }

    /// A `*time.Time`: Time.UnmarshalJSON reads the value's text as written.
    fn time(v: &JsonValue, into: &mut Option<shards_dockerfile::go::Time>) -> Result<(), Abort> {
        match v {
            JsonValue::Null => *into = None,
            JsonValue::String(s, raw) => {
                *into =
                    Some(shards_dockerfile::go::parse_rfc3339(raw.as_deref().unwrap_or(s)).map_err(Abort)?);
            }
            _ => return Err(Abort(b"Time.UnmarshalJSON: input is not a JSON string".to_vec())),
        }
        Ok(())
    }

    fn config(&mut self, v: &JsonValue, at: At, into: &mut OciImage) {
        for (name, at, m) in self.members(v, at, "ImageConfig", "v1.ImageConfig", CONFIG_FIELDS_OCI) {
            match name {
                "User" => self.string(m, at, &mut into.user, "string"),
                "Env" => self.strings(m, at, &mut into.env),
                "Volumes" => self.set(m, at, Some(&mut into.volumes)),
                "ExposedPorts" => self.set(m, at, None),
                "WorkingDir" => self.string(m, at, &mut into.working_dir, "string"),
                "Labels" => self.map(m, at, &mut into.labels),
                "Entrypoint" | "Cmd" => self.check_strings(m, at, "string"),
                "ArgsEscaped" => self.check_bool(m, at),
                _ => self.check_string(m, at, "string"),
            }
        }
    }

    fn image(&mut self, v: &JsonValue, into: &mut OciImage) -> Result<(), Abort> {
        for (name, at, m) in self.members(v, DOCUMENT, "Image", "v1.Image", IMAGE_FIELDS) {
            match name {
                "created" => Self::time(m, &mut into.created)?,
                "os.features" => self.check_strings(m, at, "string"),
                "config" => self.config(m, at, into),
                "rootfs" => {
                    for (name, at, m) in self.members(m, at, "RootFS", "v1.RootFS", ROOTFS_FIELDS) {
                        if name == "type" {
                            self.check_string(m, at, "string");
                        } else {
                            self.check_strings(m, at, "digest.Digest");
                        }
                    }
                }
                "history" => match m {
                    JsonValue::Null => {}
                    JsonValue::Array(items) => {
                        for item in items {
                            for (name, at, m) in
                                self.members(item, at, "History", "v1.History", HISTORY_FIELDS)
                            {
                                match name {
                                    "created" => Self::time(m, &mut None)?,
                                    "empty_layer" => self.check_bool(m, at),
                                    _ => self.check_string(m, at, "string"),
                                }
                            }
                        }
                    }
                    _ => self.mismatch(m, at, "[]v1.History"),
                },
                // author and the platform's strings.
                _ => self.check_string(m, at, "string"),
            }
        }
        Ok(())
    }
}

impl OciImage {
    /// `json.Unmarshal(raw, &ocispecs.Image{})`'s reading, or its error.
    fn from_json(raw: &[u8]) -> Result<OciImage, Vec<u8>> {
        let v = shards_dockerfile::json::parse(raw)?;
        let mut image = OciImage::default();
        let mut r = Reader::default();
        r.image(&v, &mut image).map_err(|Abort(e)| e)?;
        match r.saved {
            Some(e) => Err(e.into_bytes()),
            None => Ok(image),
        }
    }
}

/// `Time.Format(time.RFC3339)`: to the second, `Z` for UTC, else the offset.
fn rfc3339(t: &shards_dockerfile::go::Time) -> String {
    let mut whole = *t;
    whole.nanosecond = 0;
    whole.rfc3339_nano().unwrap_or_default()
}

/// gitutil.ParseURL of a git source's URL, a transport's or else `https://`'s.
fn parse_git_url(url: &str) -> Result<shards_dockerfile::git::GitUrl, String> {
    let url = if shards_dockerfile::git::is_git_transport(url.as_bytes()) {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    shards_dockerfile::git::parse_url(url.as_bytes()).map_err(|e| match e {
        shards_dockerfile::git::UrlError::UnknownProtocol => "unknown protocol".to_string(),
        shards_dockerfile::git::UrlError::Other(e) => String::from_utf8_lossy(&e).into_owned(),
    })
}

/// sourceToInput's `git` case: what the identifier, its full URL and the metadata say,
/// and the fields left unknown.
fn git_input(source: &Source, rest: &str, meta: &Meta) -> Result<(Git, Vec<&'static str>), String> {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let u = parse_git_url(rest)?;
    let mut g = Git {
        schema: text(&u.scheme),
        remote: text(&u.remote),
        host: text(&u.host),
        ..Git::default()
    };
    let mut reference = String::new();
    let mut full_ref = false;
    if let Some((r, sub)) = &u.opts {
        reference = text(r);
        g.subdir = text(sub);
        let cleaned = String::from_utf8_lossy(&shards_dockerfile::go::clean(sub)).into_owned();
        if cleaned == "/" || cleaned == "." {
            g.subdir.clear();
        }
    }
    if let Some(full) = source.attrs.get("git.fullurl") {
        let v = if shards_dockerfile::git::is_git_transport(full.as_bytes()) {
            full.clone()
        } else {
            format!("https://{full}")
        };
        let f = parse_git_url(&v)?;
        g.schema = text(&f.scheme);
        g.remote = text(&f.remote);
        g.host = text(&f.host);
        g.full_url = v;
    }
    if let Some(t) = reference.strip_prefix("refs/tags/") {
        g.tag_name = t.to_string();
        full_ref = true;
    }
    if let Some(b) = reference.strip_prefix("refs/heads/") {
        g.branch = b.to_string();
        full_ref = true;
    }
    if shards_git::remote::is_commit_name(&reference) {
        g.is_commit_ref = true;
        g.checksum = reference.clone();
        g.commit_checksum = reference.clone();
        full_ref = true;
    }
    let mut unknowns: Vec<&'static str> = Vec::new();
    match &meta.git {
        None => {
            if !full_ref {
                unknowns.extend(["tagName", "branch", "ref"]);
            } else {
                g.reference = reference;
            }
            if g.checksum.is_empty() {
                unknowns.extend(["checksum", "isAnnotatedTag", "commitChecksum", "isSHA256"]);
            }
            unknowns.extend(["tag", "commit"]);
        }
        Some(m) => {
            g.reference = m.reference.clone();
            if let Some(t) = g.reference.strip_prefix("refs/tags/") {
                g.tag_name = t.to_string();
            }
            if let Some(b) = g.reference.strip_prefix("refs/heads/") {
                g.branch = b.to_string();
            }
            g.checksum = m.checksum.clone();
            g.commit_checksum = if m.commit_checksum.is_empty() {
                g.checksum.clone()
            } else {
                m.commit_checksum.clone()
            };
            if g.checksum != g.commit_checksum {
                g.is_annotated_tag = true;
            }
            match &m.commit_object {
                None => unknowns.extend(["commit", "tag"]),
                Some(raw) => {
                    let obj = gitobject::parse(raw)?;
                    obj.verify_checksum(&g.commit_checksum)?;
                    let commit = obj.to_commit()?;
                    let signature = shards_gitsign::summary(&obj.signature);
                    g.commit = Some(CommitInput {
                        commit,
                        signature,
                        object: obj,
                    });
                    if let Some(raw) = m.tag_object.as_ref().filter(|t| !t.is_empty()) {
                        let obj = gitobject::parse(raw)?;
                        obj.verify_checksum(&g.checksum)?;
                        let tag = obj.to_tag()?;
                        let signature = shards_gitsign::summary(&obj.signature);
                        g.tag = Some(TagInput {
                            tag,
                            signature,
                            object: obj,
                        });
                    }
                }
            }
        }
    }
    if g.checksum.len() == 64 {
        g.is_sha256 = true;
    }
    Ok((g, unknowns))
}
