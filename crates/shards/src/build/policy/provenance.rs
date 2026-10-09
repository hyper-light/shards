//! An image's provenance as buildx v0.37.1 reads it into a policy's input
//! (policy/provenance.go, materials.go, input.go, resolve.go, input_unknowns.go): the
//! SLSA v1 or v0.2 statement among its attestations, its materials as the inputs they
//! name, and the unknowns of those materials resolved.
//!
//! buildx decodes a statement with encoding/json into BuildKit's provenance types, and
//! any field of the wrong type, anywhere in them (an LLB op's mount, a resource sample),
//! fails the decoding and leaves no provenance. So a statement is checked whole against
//! those types, as `provenance-schema.json` lays them out (reflect and typeFields' view
//! of them, written by scripts/policy/generate-provenance), and the fields buildx reads
//! are then decoded as Go decodes them: a key matched exactly, else case-folded; a later
//! key decoding into what an earlier one left, maps and structs merged and a slice's
//! elements reused; null clearing pointers, maps and slices and leaving the rest.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use shards_cmdline::buildflags::LogLevel;
use shards_dockerfile::platform::{self, Platform};
use shards_image::reference::{Digest, Reference};
use shards_sigstore::image::{SLSA_V1, SLSA_V02};

use super::input::{self, Fields, Input, Json};
use super::signatures::Trust;
use super::{AttestationChain, Meta, MetaRequest, Resolve, Source};

/// The annotation naming an attestation's predicate type.
const PREDICATE_TYPE: &str = "in-toto.io/predicate-type";
/// maxMaterialDepth.
const MAX_MATERIAL_DEPTH: i64 = 24;
/// The scanner's maxNestingDepth.
const MAX_NESTING: usize = 10_000;

/// What buildx logs as it reads, by level.
pub type Log<'a> = dyn FnMut(LogLevel, &str) + 'a;

// ---- the input ----

/// `ImageProvenance`.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    pub predicate_type: String,
    pub build_type: String,
    pub builder_id: String,
    pub invocation_id: String,
    pub started_on: String,
    pub finished_on: String,
    pub config_uri: String,
    pub config_digest: BTreeMap<String, String>,
    pub config_path: String,
    pub frontend: String,
    pub build_args: BTreeMap<String, String>,
    pub raw_args: BTreeMap<String, String>,
    pub reproducible: Option<bool>,
    pub hermetic: Option<bool>,
    pub completeness: Option<Completeness>,
    /// The materials as inputs, one for each of `materials_raw`.
    pub materials: Vec<Input>,
    /// The materials buildx could read a source from (materialsRaw).
    pub materials_raw: Vec<Material>,
}

/// `ImageProvenanceCompleteness`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Completeness {
    pub parameters: Option<bool>,
    pub environment: Option<bool>,
    pub materials: Option<bool>,
}

/// A material's URI and digests (slsa1.ResourceDescriptor, as materialsRaw keeps them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Material {
    pub uri: String,
    pub digest: BTreeMap<String, String>,
}

impl Provenance {
    pub fn json(&self) -> Json {
        let config = Fields::default()
            .str("uri", &self.config_uri)
            .map("digest", &self.config_digest)
            .str("path", &self.config_path)
            .done();
        let completeness = self.completeness.map(|c| {
            Fields::default()
                .json("parameters", c.parameters.map(Json::Bool))
                .json("environment", c.environment.map(Json::Bool))
                .json("materials", c.materials.map(Json::Bool))
                .done()
        });
        Fields::default()
            .str("predicateType", &self.predicate_type)
            .str("buildType", &self.build_type)
            .str("builderID", &self.builder_id)
            .str("invocationID", &self.invocation_id)
            .str("startedOn", &self.started_on)
            .str("finishedOn", &self.finished_on)
            .json("configSource", Some(config))
            .str("frontend", &self.frontend)
            .map("buildArgs", &self.build_args)
            .map("rawArgs", &self.raw_args)
            .json("reproducible", self.reproducible.map(Json::Bool))
            .json("hermetic", self.hermetic.map(Json::Bool))
            .json("completeness", completeness)
            .json(
                "materials",
                (!self.materials.is_empty())
                    .then(|| Json::Arr(self.materials.iter().map(Input::json).collect())),
            )
            .done()
    }
}

/// `SourceToInput`: the input for `source` as `meta` tells it, for `platform`, with its
/// provenance's materials as inputs of their own (sourceToInputRecursive).
pub fn source_to_input(
    source: &Source,
    meta: &Meta,
    platform: Option<&Platform>,
    trust: Option<&Trust>,
    log: &mut Log<'_>,
) -> Result<Input, String> {
    let mut seen = Vec::new();
    recursive(source, meta, platform, 0, &mut seen, trust, log)
}

fn recursive(
    source: &Source,
    meta: &Meta,
    platform: Option<&Platform>,
    depth: i64,
    seen: &mut Vec<String>,
    trust: Option<&Trust>,
    log: &mut Log<'_>,
) -> Result<Input, String> {
    if depth > MAX_MATERIAL_DEPTH {
        return Err(format!(
            "provenance materials depth exceeds limit {MAX_MATERIAL_DEPTH}"
        ));
    }
    let mut inp = input::of_source(source, meta, platform, trust, log)?;
    inp.env.depth = depth;
    let raw = match inp.image.as_ref().and_then(|i| i.provenance.as_ref()) {
        Some(p) if !p.materials_raw.is_empty() => p.materials_raw.clone(),
        _ => return Ok(inp),
    };
    // Cycle detection: a source met again on the way down is an empty input.
    let key = unique_identifier(source, platform);
    if seen.contains(&key) {
        return Ok(Input::default());
    }
    seen.push(key);
    let materials = materials_of(&raw, platform, depth, seen, trust, log);
    seen.pop();
    if let Some(p) = inp.image.as_mut().and_then(|i| i.provenance.as_mut()) {
        p.materials = materials?;
    }
    Ok(inp)
}

fn materials_of(
    raw: &[Material],
    platform: Option<&Platform>,
    depth: i64,
    seen: &mut Vec<String>,
    trust: Option<&Trust>,
    log: &mut Log<'_>,
) -> Result<Vec<Input>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for m in raw {
        let Ok((src, mat_platform)) = parse_material(m) else {
            out.push(Input::default());
            continue;
        };
        let child = recursive(
            &src,
            &Meta::default(),
            mat_platform.as_ref().or(platform),
            depth + 1,
            seen,
            trust,
            log,
        )
        .map_err(|e| {
            format!(
                "failed to build material input for {}: {e}",
                shards_cmdline::go::quote(&m.uri)
            )
        })?;
        out.push(child);
    }
    Ok(out)
}

/// sourceUniqueIdentifier: the identifier, and the platform where there is one.
fn unique_identifier(source: &Source, platform: Option<&Platform>) -> String {
    match platform {
        Some(p) => format!(
            "{}|{}",
            source.identifier,
            String::from_utf8_lossy(&platform::format(p))
        ),
        None => source.identifier.clone(),
    }
}

/// applyEnvWithDepth: the build's `env` on the input at `depth`, and on each material
/// one deeper.
pub fn apply_env(inp: &mut Input, env: &input::Env, depth: i64) {
    inp.env = env.clone();
    inp.env.depth = depth;
    if let Some(p) = inp.image.as_mut().and_then(|i| i.provenance.as_mut()) {
        for m in &mut p.materials {
            apply_env(m, env, depth + 1);
        }
    }
}

// ---- resolving the materials' unknowns ----

/// `ResolveInputUnknowns`: what of `unknowns` the input's own source must be asked for
/// (the request for BuildKit), or, for a material's, its metadata resolved through
/// `resolver` and the material read again (`true`: evaluate again).
#[allow(clippy::too_many_arguments)]
pub fn resolve_input_unknowns(
    inp: &mut Input,
    root: &Source,
    unknowns: &[String],
    root_platform: Option<&Platform>,
    default_platform: Option<&Platform>,
    resolver: Option<&dyn Resolve>,
    trust: Option<&Trust>,
    log: &mut Log<'_>,
) -> Result<(bool, Option<MetaRequest>), String> {
    if unknowns.is_empty() {
        return Ok((false, None));
    }
    let walk = Walk {
        root_platform,
        default_platform,
        resolver,
        trust,
    };
    walk.resolve(inp, root, default_platform, &normalize(unknowns), true, log)
}

struct Walk<'a> {
    root_platform: Option<&'a Platform>,
    default_platform: Option<&'a Platform>,
    resolver: Option<&'a dyn Resolve>,
    trust: Option<&'a Trust>,
}

impl Walk<'_> {
    /// resolveNodeUnknowns: the node's own unknowns first, then its materials', each in
    /// index order (Go's map gives them in any).
    fn resolve(
        &self,
        node: &mut Input,
        source: &Source,
        node_platform: Option<&Platform>,
        unknowns: &[String],
        root: bool,
        log: &mut Log<'_>,
    ) -> Result<(bool, Option<MetaRequest>), String> {
        let (direct, children) = split_unknowns(unknowns);
        if !direct.is_empty()
            && let Some(req) = self.request(node_platform, root, &direct, log)?
        {
            if root {
                return Ok((false, Some(req)));
            }
            let resolver = self
                .resolver
                .ok_or("material metadata resolution requires source resolver")?;
            // ToResolverOpt: the request's platform, else the default.
            let mut req = req;
            if req.platform.is_none() {
                req.platform = self.default_platform.cloned();
            }
            let meta = resolver
                .resolve(source, &req)
                .map_err(|e| format!("failed to resolve source metadata for material: {e}"))?;
            let next = source_to_input(source, &meta, node_platform, self.trust, log)
                .map_err(|e| format!("failed to rebuild material input: {e}"))?;
            *node = next;
            return Ok((true, None));
        }
        for (idx, child) in children {
            let Some(p) = node.image.as_mut().and_then(|i| i.provenance.as_mut()) else {
                continue;
            };
            let (Some(raw), Some(material)) = (p.materials_raw.get(idx), p.materials.get_mut(idx)) else {
                continue;
            };
            let Ok((child_source, child_platform)) = parse_material(raw) else {
                continue;
            };
            let platform = child_platform.or_else(|| node_platform.cloned());
            let (retry, next) =
                self.resolve(material, &child_source, platform.as_ref(), &child, false, log)?;
            if retry || next.is_some() {
                return Ok((retry, next));
            }
        }
        Ok((false, None))
    }

    /// sourceResolveRequest: what to ask of a node's source for `fields`, if anything.
    fn request(
        &self,
        node_platform: Option<&Platform>,
        root: bool,
        fields: &[String],
        log: &mut Log<'_>,
    ) -> Result<Option<MetaRequest>, String> {
        let platform = match node_platform {
            Some(p) => Some(Platform {
                os: p.os.clone(),
                architecture: p.architecture.clone(),
                variant: p.variant.clone(),
                ..Platform::default()
            }),
            None if root => self.root_platform.cloned(),
            None => None,
        };
        let mut req = MetaRequest {
            platform,
            ..MetaRequest::default()
        };
        add_unknowns(fields, &mut req, log)?;
        Ok(req.asks().then_some(req))
    }
}

/// AddUnknownsWithLogger: the request for `unknowns`, those it collected logged.
pub fn add_unknowns(unknowns: &[String], req: &mut MetaRequest, log: &mut Log<'_>) -> Result<(), String> {
    let collected: Vec<&str> = unknowns
        .iter()
        .map(String::as_str)
        .filter(|u| !matches!(*u, "image" | "git" | "http" | "local"))
        .collect();
    if collected.is_empty() {
        return Ok(());
    }
    log(
        LogLevel::Debug,
        &format!("collected unknowns: [{}]", collected.join(" ")),
    );
    super::request_for(unknowns, req)
}

/// normalizeNodeUnknowns: without `input.`, empty ones dropped, each once.
fn normalize(unknowns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in unknowns {
        let v = u.strip_prefix("input.").unwrap_or(u);
        if !v.is_empty() && !out.iter().any(|o| o == v) {
            out.push(v.to_string());
        }
    }
    out
}

/// splitNodeUnknowns: the node's own unknowns, and its materials', by index.
fn split_unknowns(unknowns: &[String]) -> (Vec<String>, BTreeMap<usize, Vec<String>>) {
    let mut direct: Vec<String> = Vec::new();
    let mut child: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for u in unknowns {
        if u.is_empty() {
            continue;
        }
        if let Some((idx, rest)) = material_key(u) {
            let (Ok(idx), false) = (usize::try_from(idx), rest.is_empty()) else {
                continue;
            };
            let c = child.entry(idx).or_default();
            if !c.iter().any(|x| x == rest) {
                c.push(rest.to_string());
            }
            continue;
        }
        if !direct.contains(u) {
            direct.push(u.clone());
        }
    }
    (direct, child)
}

/// isMaterialKey: `image.provenance.materials[N]` and what follows it.
fn material_key(key: &str) -> Option<(i64, &str)> {
    let rest = key.strip_prefix("image.provenance.materials[")?;
    let (n, rest) = rest.split_once(']')?;
    let n = n.parse::<i64>().ok()?;
    Some((n, rest.strip_prefix('.').unwrap_or(rest)))
}

// ---- the provenance ----

/// parseProvenance: the first SLSA v1 or v0.2 attestation in the chain that reads as a
/// provenance.
pub fn parse(chain: &AttestationChain, log: &mut Log<'_>) -> Result<Option<Provenance>, String> {
    for (desc, data) in chain.blobs.values() {
        if data.is_empty() {
            continue;
        }
        let Some(pt) = desc.annotations.get(PREDICATE_TYPE).filter(|p| !p.is_empty()) else {
            continue;
        };
        if pt != SLSA_V02 && pt != SLSA_V1 {
            continue;
        }
        if let Some(p) = parse_blob(data, pt, log)? {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// parseProvenanceBlob: an in-toto statement's predicate, by its type, else the
/// annotation's.
fn parse_blob(data: &[u8], annotated: &str, log: &mut Log<'_>) -> Result<Option<Provenance>, String> {
    let schema = schema()?;
    let Some(doc) = Doc::parse(data) else {
        return Ok(None);
    };
    if !schema.check(&doc, doc.root, schema.statement) {
        return Ok(None);
    }
    let mut predicate_type = String::new();
    let mut predicate = None;
    for (name, n, _) in schema.members(&doc, doc.root, schema.statement) {
        match name {
            "predicateType" => set_str(&doc, n, &mut predicate_type),
            "predicate" => predicate = Some(n),
            _ => {}
        }
    }
    let Some(predicate) = predicate else {
        return Ok(None);
    };
    let t = if predicate_type.is_empty() {
        annotated
    } else {
        predicate_type.as_str()
    };
    let provenance = if t == SLSA_V1 {
        if !schema.check(&doc, predicate, schema.slsa1) {
            return Ok(None);
        }
        slsa1(schema.v1(&doc, predicate), log)
    } else if t == SLSA_V02 {
        if !schema.check(&doc, predicate, schema.slsa02) {
            return Ok(None);
        }
        slsa02(schema.v02(&doc, predicate), log)
    } else {
        None
    };
    Ok(provenance)
}

/// parseSLSA1Provenance.
fn slsa1(p: V1, log: &mut Log<'_>) -> Option<Provenance> {
    if p.build_type.is_empty() && p.builder_id.is_empty() {
        return None;
    }
    let mut prv = Provenance {
        predicate_type: SLSA_V1.to_string(),
        build_type: p.build_type,
        builder_id: p.builder_id,
        config_uri: p.config.uri,
        config_digest: p.config.digest.unwrap_or_default(),
        config_path: p.config.path,
        frontend: p.frontend,
        build_args: build_args(p.args.as_ref()),
        raw_args: p.args.unwrap_or_default(),
        materials_raw: raw_materials(p.deps.live(), log),
        ..Provenance::default()
    };
    if let Some(md) = p.meta {
        prv.invocation_id = md.invocation_id;
        prv.started_on = provenance_time(md.started.as_ref());
        prv.finished_on = provenance_time(md.finished.as_ref());
        prv.reproducible = Some(md.reproducible);
        prv.hermetic = Some(md.hermetic);
        prv.completeness = Some(Completeness {
            parameters: Some(md.parameters),
            environment: None,
            materials: Some(md.materials),
        });
    }
    Some(prv)
}

/// parseSLSA02Provenance.
fn slsa02(p: V02, log: &mut Log<'_>) -> Option<Provenance> {
    if p.build_type.is_empty() && p.builder_id.is_empty() {
        return None;
    }
    let mut prv = Provenance {
        predicate_type: SLSA_V02.to_string(),
        build_type: p.build_type,
        builder_id: p.builder_id,
        config_uri: p.config.uri,
        config_digest: p.config.digest.unwrap_or_default(),
        config_path: p.config.path,
        frontend: p.frontend,
        build_args: build_args(p.args.as_ref()),
        raw_args: p.args.unwrap_or_default(),
        materials_raw: raw_materials(p.materials.live(), log),
        ..Provenance::default()
    };
    if let Some(md) = p.meta {
        prv.invocation_id = md.invocation_id;
        prv.started_on = provenance_time(md.started.as_ref());
        prv.finished_on = provenance_time(md.finished.as_ref());
        prv.reproducible = Some(md.reproducible);
        prv.hermetic = Some(md.hermetic);
        prv.completeness = Some(Completeness {
            parameters: Some(md.parameters),
            environment: Some(md.environment),
            materials: Some(md.materials),
        });
    }
    Some(prv)
}

/// rawMaterialsFromSLSA1 and rawMaterialsFromSLSA02: the materials a source can be read
/// from; the others logged and skipped.
fn raw_materials(deps: &[Dep], log: &mut Log<'_>) -> Vec<Material> {
    let mut out = Vec::new();
    for d in deps {
        let m = Material {
            uri: d.uri.clone(),
            digest: d.digest.clone().unwrap_or_default(),
        };
        if let Err(e) = parse_material(&m) {
            log(
                LogLevel::Warn,
                &format!(
                    "skipping unsupported provenance material {}: {e}",
                    shards_cmdline::go::quote(&m.uri)
                ),
            );
            continue;
        }
        out.push(m);
    }
    out
}

/// extractBuildArgs: the `build-arg:NAME` arguments, by name.
fn build_args(args: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    args.into_iter()
        .flatten()
        .filter_map(|(k, v)| {
            k.strip_prefix("build-arg:")
                .filter(|n| !n.is_empty())
                .map(|n| (n.to_string(), v.clone()))
        })
        .collect()
}

/// formatProvenanceTime: in UTC, to the second (`t.UTC().Format(time.RFC3339)`).
fn provenance_time(t: Option<&shards_dockerfile::go::Time>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    let (secs, _) = t.unix();
    let u = shards_dockerfile::go::Time::from_unix(secs);
    // appendInt(year, 4): a sign, then at least four digits.
    let year = if u.year < 0 {
        format!("-{:04}", u.year.unsigned_abs())
    } else {
        format!("{:04}", u.year)
    };
    format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        u.month, u.day, u.hour, u.minute, u.second
    )
}

// ---- materials ----

/// parseSLSAMaterial: the source a material names, and the platform its purl names.
pub fn parse_material(m: &Material) -> Result<(Source, Option<Platform>), String> {
    let q = shards_cmdline::go::quote;
    if m.uri.starts_with("pkg:docker/") {
        return docker_material(&m.uri, &m.digest);
    }
    if let Ok(gu) = shards_dockerfile::git::parse_url(m.uri.as_bytes()) {
        let path = go_lower(&String::from_utf8_lossy(&gu.path));
        if path.ends_with(".git") || (gu.scheme != b"http" && gu.scheme != b"https") {
            return git_material(&m.uri);
        }
    }
    if m.uri.starts_with("https://") || m.uri.starts_with("http://") {
        return Ok((Source::new(m.uri.clone()), None));
    }
    Err(format!("unsupported material URI {}", q(&m.uri)))
}

/// dockerMaterialSource: the image a purl names, pinned to the material's digest.
fn docker_material(
    uri: &str,
    digest: &BTreeMap<String, String>,
) -> Result<(Source, Option<Platform>), String> {
    let q = shards_cmdline::go::quote;
    let (reference, platform) = purl_to_ref(uri)?;
    let mut named = Reference::parse_normalized(&reference)
        .map_err(|e| format!("invalid docker reference {} from {}: {e}", q(&reference), q(uri)))?;
    if let Some(checksum) = digest.get("sha256").map(|d| d.trim()).filter(|d| !d.is_empty()) {
        let d = Digest::parse(checksum)
            .or_else(|_| Digest::parse(&format!("sha256:{checksum}")))
            .map_err(|e| format!("invalid material digest {} for {}: {e}", q(checksum), q(uri)))?;
        match &named.digest {
            Some(have) if *have != d => {
                return Err(format!(
                    "material digest mismatch for {}: ref has {have} but provenance has {d}",
                    q(uri)
                ));
            }
            Some(_) => {}
            None => named.digest = Some(d),
        }
    }
    Ok((Source::new(format!("docker-image://{named}")), platform))
}

/// gitMaterialSource: a git source, its full URL the material's.
fn git_material(uri: &str) -> Result<(Source, Option<Platform>), String> {
    let gu = shards_dockerfile::git::parse_url(uri.as_bytes()).map_err(|e| match e {
        shards_dockerfile::git::UrlError::UnknownProtocol => "unknown protocol".to_string(),
        shards_dockerfile::git::UrlError::Other(e) => String::from_utf8_lossy(&e).into_owned(),
    })?;
    if !matches!(gu.scheme.as_slice(), b"https" | b"http" | b"ssh" | b"git") {
        return Err(format!(
            "unsupported git material URI {}",
            shards_cmdline::go::quote(uri)
        ));
    }
    let mut id = gu.host.clone();
    id.extend_from_slice(&shards_dockerfile::go::join(&[b"/", &gu.path]));
    if let Some((r, sub)) = &gu.opts
        && (!r.is_empty() || !sub.is_empty())
    {
        id.push(b'#');
        id.extend_from_slice(r);
        if !sub.is_empty() {
            id.push(b':');
            id.extend_from_slice(sub);
        }
    }
    let mut source = Source::new(format!("git://{}", String::from_utf8_lossy(&id)));
    source.attrs.insert("git.fullurl".into(), uri.to_string());
    Ok((source, None))
}

/// strings.ToLower: each character's simple lowercase mapping (`İ` to `i`, where full
/// lowercasing gives two).
fn go_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\u{130}' {
                return 'i';
            }
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(one), None) => one,
                _ => c,
            }
        })
        .collect()
}

/// A package URL as packageurl-go v0.1.5's FromString reads it: strings are Go's, bytes.
struct PackageUrl {
    kind: Vec<u8>,
    namespace: Vec<u8>,
    name: Vec<u8>,
    version: Vec<u8>,
    qualifiers: Vec<(Vec<u8>, Vec<u8>)>,
}

fn split_bytes(s: &[u8], sep: u8) -> impl Iterator<Item = &[u8]> {
    s.split(move |&b| b == sep)
}

fn split_once(s: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&b| b == sep)?;
    Some((s.get(..i)?, s.get(i + 1..)?))
}

fn trim_slashes_left(mut s: &[u8]) -> &[u8] {
    while let Some(rest) = s.strip_prefix(b"/") {
        s = rest;
    }
    s
}

/// `packageurl.FromString`.
fn package_url(purl: &str) -> Result<PackageUrl, String> {
    let b = purl.as_bytes();
    let mut remainder = b;
    if let Some((left, right)) = split_once(b, b'#') {
        remainder = left;
        let mut right = trim_slashes_left(right);
        while let Some(rest) = right.strip_suffix(b"/") {
            right = rest;
        }
        for item in split_bytes(right, b'/') {
            let item: Vec<u8> = item.iter().copied().filter(|&c| c != b'.').collect();
            if !item.is_empty() {
                path_unescape(&item).map_err(|e| format!("failed to unescape path: {e}"))?;
            }
        }
    }
    let mut qualifiers = Vec::new();
    if let Some(index) = remainder.iter().rposition(|&c| c == b'?') {
        let qualifier = remainder.get(index + 1..).unwrap_or_default();
        for item in split_bytes(qualifier, b'&') {
            let mut kv = split_bytes(item, b'=');
            let key = go_lower(&String::from_utf8_lossy(kv.next().unwrap_or_default()));
            let key = path_unescape(key.as_bytes())
                .map_err(|e| format!("failed to unescape qualifier key: {e}"))?;
            if !valid_qualifier_key(&key) {
                return Err(format!(
                    "invalid qualifier key: '{}'",
                    String::from_utf8_lossy(&key)
                ));
            }
            // packageurl-go reads kv[1] even where there is none, and panics.
            let Some(value) = kv.next() else {
                return Err(format!(
                    "invalid qualifier {}: no value",
                    shards_dockerfile::go::quote(item)
                ));
            };
            if value.is_empty() {
                continue;
            }
            let value =
                path_unescape(value).map_err(|e| format!("failed to unescape qualifier value: {e}"))?;
            qualifiers.push((key, value));
        }
        remainder = remainder.get(..index).unwrap_or_default();
    }
    let Some((_, rest)) = split_once(remainder, b':').filter(|(s, _)| *s == b"pkg") else {
        return Err("scheme is missing".into());
    };
    let remainder = trim_slashes_left(rest);
    let Some((kind, remainder)) = split_once(remainder, b'/') else {
        return Err("type is missing".into());
    };
    let kind = go_lower(&String::from_utf8_lossy(kind)).into_bytes();
    let index = remainder.iter().rposition(|&c| c == b'/');
    let mut name = match index {
        Some(i) => remainder.get(i + 1..).unwrap_or_default().to_vec(),
        None => remainder.to_vec(),
    };
    let mut version = Vec::new();
    if let Some((n, v)) = split_once(&name.clone(), b'@') {
        version = path_unescape(v).map_err(|e| format!("failed to unescape purl version: {e}"))?;
        name = path_unescape(n).map_err(|e| format!("failed to unescape purl name: {e}"))?;
    }
    let mut namespaces = Vec::new();
    if let Some(i) = index {
        for item in split_bytes(remainder.get(..i).unwrap_or_default(), b'/') {
            if !item.is_empty() {
                namespaces.push(path_unescape(item).map_err(|e| format!("failed to unescape path: {e}"))?);
            }
        }
    }
    if name.is_empty() {
        return Err("name is required".into());
    }
    Ok(PackageUrl {
        kind,
        namespace: namespaces.join(&b'/'),
        name,
        version,
        qualifiers,
    })
}

/// `^[A-Za-z\.\-_][0-9A-Za-z\.\-_]*$`.
fn valid_qualifier_key(key: &[u8]) -> bool {
    let ok = |c: &u8| c.is_ascii_alphabetic() || matches!(c, b'.' | b'-' | b'_');
    key.first().is_some_and(ok) && key.iter().all(|c| ok(c) || c.is_ascii_digit())
}

/// url.PathUnescape.
fn path_unescape(s: &[u8]) -> Result<Vec<u8>, String> {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if c != b'%' {
            out.push(c);
            i += 1;
            continue;
        }
        match (
            s.get(i + 1).copied().and_then(hex),
            s.get(i + 2).copied().and_then(hex),
        ) {
            (Some(h), Some(l)) => {
                out.push(u8::try_from(h * 16 + l).unwrap_or_default());
                i += 3;
            }
            _ => {
                let bad = s.get(i..(i + 3).min(s.len())).unwrap_or_default();
                return Err(format!(
                    "invalid URL escape {}",
                    shards_dockerfile::go::quote(bad)
                ));
            }
        }
    }
    Ok(out)
}

/// go-digest's Parse of a Go string, which may hold bytes that are no UTF-8: those fail
/// as its checks fail them.
fn parse_digest(b: &[u8]) -> Result<Digest, String> {
    if let Ok(s) = std::str::from_utf8(b) {
        return Digest::parse(s).map_err(|e| e.to_string());
    }
    let colon = b
        .iter()
        .position(|&c| c == b':')
        .filter(|&i| i > 0 && i + 1 < b.len());
    let size = match colon.and_then(|i| b.get(..i)) {
        Some(b"sha256") => 64,
        Some(b"sha384") => 96,
        Some(b"sha512") => 128,
        _ => return Err("invalid checksum digest format".into()),
    };
    let encoded = colon.and_then(|i| b.get(i + 1..)).unwrap_or_default();
    if encoded.len() != size {
        return Err("invalid checksum digest length".into());
    }
    Err("invalid checksum digest format".into())
}

/// purl.PURLToRef: the image reference a docker purl names, and the platform it names.
fn purl_to_ref(purl: &str) -> Result<(String, Option<Platform>), String> {
    let q = shards_dockerfile::go::quote;
    let p = package_url(purl)?;
    if p.kind != b"docker" {
        return Err(format!("invalid package type {}, expecting docker", q(&p.kind)));
    }
    let mut r = p.name.clone();
    if !p.namespace.is_empty() {
        r = [p.namespace.as_slice(), b"/", &r].concat();
    }
    let mut version_digest: Option<String> = None;
    if !p.version.is_empty() {
        match parse_digest(&p.version) {
            Ok(d) => {
                r.push(b'@');
                r.extend_from_slice(d.to_string().as_bytes());
                version_digest = Some(d.to_string());
            }
            Err(_) => {
                r.push(b':');
                r.extend_from_slice(&p.version);
            }
        }
    }
    let mut platform = None;
    for (key, value) in &p.qualifiers {
        if key == b"platform" {
            let mut pl = platform::parse(value, &crate::build::host_platform())
                .map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
            pl.os_version.clear();
            pl.os_features.clear();
            platform = Some(pl);
        }
        if key == b"digest" {
            if let Some(v) = &version_digest {
                if v.as_bytes() != value.as_slice() {
                    return Err(format!(
                        "digest {} does not match version {}",
                        q(value),
                        q(v.as_bytes())
                    ));
                }
                continue;
            }
            let d = parse_digest(value)?;
            r.push(b'@');
            r.extend_from_slice(d.to_string().as_bytes());
            version_digest = Some(d.to_string());
        }
    }
    if version_digest.is_none() && p.version.is_empty() {
        r.extend_from_slice(b":latest");
    }
    let named = Reference::parse_normalized(&String::from_utf8_lossy(&r))
        .map_err(|e| format!("invalid image url {}: {e}", shards_cmdline::go::quote(purl)))?;
    Ok((named.to_string(), platform))
}

// ---- JSON as encoding/json reads it ----

#[derive(Debug)]
enum Kind {
    Null,
    Bool(bool),
    Number,
    Str(String),
    Arr(Vec<usize>),
    Obj(Vec<(String, usize)>),
}

/// A value and where its text is.
#[derive(Debug)]
struct Node {
    kind: Kind,
    start: usize,
    end: usize,
}

/// A document as Go's scanner checks it and its decoder reads it: its values in one
/// flat list, so no nesting takes stack.
#[derive(Debug)]
struct Doc<'a> {
    text: &'a [u8],
    nodes: Vec<Node>,
    root: usize,
}

fn ws(text: &[u8], mut at: usize) -> usize {
    while matches!(text.get(at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        at += 1;
    }
    at
}

impl<'a> Doc<'a> {
    /// The document, or `None` where Go's checkValid refuses it.
    fn parse(text: &'a [u8]) -> Option<Doc<'a>> {
        let mut nodes: Vec<Node> = Vec::new();
        // Open containers, an object's with the key its next value takes.
        let mut open: Vec<(usize, Option<String>)> = Vec::new();
        let mut at = ws(text, 0);
        'value: loop {
            let start = at;
            let mut done = match *text.get(at)? {
                c @ (b'{' | b'[') => {
                    if open.len() >= MAX_NESTING {
                        return None;
                    }
                    let object = c == b'{';
                    nodes.push(Node {
                        kind: if object {
                            Kind::Obj(Vec::new())
                        } else {
                            Kind::Arr(Vec::new())
                        },
                        start,
                        end: start,
                    });
                    let idx = nodes.len() - 1;
                    at = ws(text, at + 1);
                    let close = if object { b'}' } else { b']' };
                    if text.get(at) == Some(&close) {
                        at += 1;
                        nodes.get_mut(idx)?.end = at;
                        idx
                    } else if object {
                        let (k, next) = key(text, at)?;
                        at = next;
                        open.push((idx, Some(k)));
                        continue 'value;
                    } else {
                        open.push((idx, None));
                        continue 'value;
                    }
                }
                b'"' => {
                    let (s, end) = string(text, at)?;
                    at = end;
                    nodes.push(Node {
                        kind: Kind::Str(s),
                        start,
                        end,
                    });
                    nodes.len() - 1
                }
                b't' | b'f' | b'n' => {
                    let (kind, lit): (Kind, &[u8]) = match text.get(at) {
                        Some(b't') => (Kind::Bool(true), b"true"),
                        Some(b'f') => (Kind::Bool(false), b"false"),
                        _ => (Kind::Null, b"null"),
                    };
                    if text.get(at..at + lit.len())? != lit {
                        return None;
                    }
                    at += lit.len();
                    nodes.push(Node { kind, start, end: at });
                    nodes.len() - 1
                }
                b'-' | b'0'..=b'9' => {
                    at = number(text, at)?;
                    nodes.push(Node {
                        kind: Kind::Number,
                        start,
                        end: at,
                    });
                    nodes.len() - 1
                }
                _ => return None,
            };
            // Each container the value completes, closed.
            loop {
                let Some((parent, pending)) = open.last_mut() else {
                    at = ws(text, at);
                    return (at == text.len()).then_some(Doc {
                        text,
                        nodes,
                        root: done,
                    });
                };
                let parent = *parent;
                let object = match &mut nodes.get_mut(parent)?.kind {
                    Kind::Obj(m) => {
                        m.push((pending.take()?, done));
                        true
                    }
                    Kind::Arr(a) => {
                        a.push(done);
                        false
                    }
                    _ => return None,
                };
                at = ws(text, at);
                match text.get(at) {
                    Some(b',') => {
                        at = ws(text, at + 1);
                        if object {
                            let (k, next) = key(text, at)?;
                            at = next;
                            *pending = Some(k);
                        }
                        continue 'value;
                    }
                    Some(b'}') if object => {}
                    Some(b']') if !object => {}
                    _ => return None,
                }
                at += 1;
                nodes.get_mut(parent)?.end = at;
                open.pop();
                done = parent;
            }
        }
    }

    fn node(&self, n: usize) -> Option<&Node> {
        self.nodes.get(n)
    }

    /// A number's text.
    fn number(&self, n: usize) -> &str {
        self.node(n)
            .and_then(|x| self.text.get(x.start..x.end))
            .and_then(|t| std::str::from_utf8(t).ok())
            .unwrap_or_default()
    }

    /// A string's text as written, without its quotes.
    fn raw_string(&self, n: usize) -> &[u8] {
        self.node(n)
            .and_then(|x| self.text.get(x.start + 1..x.end.saturating_sub(1)))
            .unwrap_or_default()
    }
}

/// An object's key, its colon and the space after: the key, and where its value starts.
fn key(text: &[u8], at: usize) -> Option<(String, usize)> {
    if text.get(at) != Some(&b'"') {
        return None;
    }
    let (k, end) = string(text, at)?;
    let at = ws(text, end);
    if text.get(at) != Some(&b':') {
        return None;
    }
    Some((k, ws(text, at + 1)))
}

/// A string from its opening quote: decoded, and where it ends.
fn string(text: &[u8], at: usize) -> Option<(String, usize)> {
    let mut i = at + 1;
    loop {
        match *text.get(i)? {
            b'"' => break,
            c if c < 0x20 => return None,
            b'\\' => match *text.get(i + 1)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                b'u' => {
                    if !text.get(i + 2..i + 6)?.iter().all(u8::is_ascii_hexdigit) {
                        return None;
                    }
                    i += 6;
                }
                _ => return None,
            },
            _ => i += 1,
        }
    }
    Some((unquote(text.get(at + 1..i)?), i + 1))
}

/// A number from its first character: where it ends.
fn number(text: &[u8], mut at: usize) -> Option<usize> {
    let digit = |at: usize| text.get(at).is_some_and(u8::is_ascii_digit);
    if text.get(at) == Some(&b'-') {
        at += 1;
    }
    match text.get(at)? {
        b'0' => at += 1,
        b'1'..=b'9' => {
            while digit(at) {
                at += 1;
            }
        }
        _ => return None,
    }
    if text.get(at) == Some(&b'.') {
        at += 1;
        if !digit(at) {
            return None;
        }
        while digit(at) {
            at += 1;
        }
    }
    if matches!(text.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(text.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        if !digit(at) {
            return None;
        }
        while digit(at) {
            at += 1;
        }
    }
    Some(at)
}

/// `\uXXXX` at the start of `s`.
fn u4(s: &[u8]) -> Option<u32> {
    if s.get(..2)? != b"\\u" {
        return None;
    }
    let hex = std::str::from_utf8(s.get(2..6)?).ok()?;
    u32::from_str_radix(hex, 16).ok()
}

/// One character of Go string bytes, as utf8.DecodeRune reads it: U+FFFD one byte wide
/// where they are no UTF-8.
fn decode_rune(s: &[u8]) -> (char, usize) {
    let n = match s.first() {
        Some(0xC2..=0xDF) => 2,
        Some(0xE0..=0xEF) => 3,
        Some(0xF0..=0xF4) => 4,
        _ => 0,
    };
    match s.get(..n).map(std::str::from_utf8) {
        Some(Ok(c)) if n > 0 => (c.chars().next().unwrap_or('\u{FFFD}'), n),
        _ => (char::REPLACEMENT_CHARACTER, 1),
    }
}

/// unquoteBytes of a string's text without its quotes, the scanner having checked it.
fn unquote(s: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut r = 0;
    while let Some(&c) = s.get(r) {
        if c == b'\\' {
            let rest = s.get(r..).unwrap_or_default();
            match s.get(r + 1) {
                Some(b'u') => {
                    let rr = u4(rest).unwrap_or(0xFFFD);
                    r += 6;
                    if (0xD800..0xE000).contains(&rr) {
                        let next = u4(s.get(r..).unwrap_or_default());
                        if let Some(lo) = next.filter(|lo| (0xDC00..0xE000).contains(lo))
                            && (0xD800..0xDC00).contains(&rr)
                        {
                            let c = 0x10000 + ((rr - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER));
                            r += 6;
                        } else {
                            out.push(char::REPLACEMENT_CHARACTER);
                        }
                        continue;
                    }
                    out.push(char::from_u32(rr).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                Some(&e) => {
                    out.push(match e {
                        b'b' => '\x08',
                        b'f' => '\x0c',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        other => char::from(other),
                    });
                    r += 2;
                }
                None => r += 1,
            }
        } else if c < 0x80 {
            out.push(char::from(c));
            r += 1;
        } else {
            let (ch, n) = decode_rune(s.get(r..).unwrap_or_default());
            out.push(ch);
            r += n;
        }
    }
    out
}

// ---- the Go types ----

#[derive(Debug, Clone, Copy)]
enum MapKey {
    Str,
    Int(u32),
    Uint(u32),
}

#[derive(Debug)]
struct Field {
    name: String,
    folded: String,
    ty: usize,
}

/// A Go type as encoding/json decodes into it.
#[derive(Debug)]
enum Ty {
    Str,
    Bool,
    Int(u32),
    Uint(u32),
    Float(u32),
    /// `interface{}`.
    Any,
    /// An interface with methods: nothing but null decodes into one.
    Iface,
    Ptr(usize),
    Slice(usize),
    Map(MapKey, usize),
    Struct(Vec<Field>),
    /// `time.Time`, whose UnmarshalJSON parses the text between the quotes as written.
    Time,
    /// `json.RawMessage`.
    Raw,
    /// A type whose UnmarshalJSON decodes into another (pb/json.go's jsonOp,
    /// jsonFileAction, jsonUserOpt).
    Via(usize),
    /// BuildKit's Environment and ProvenanceInternalParametersSLSA1: decoded into a
    /// `map[string]any` and into their own fields.
    Flatten(Vec<Field>),
}

#[derive(Debug)]
struct Schema {
    types: Vec<Ty>,
    statement: usize,
    slsa1: usize,
    slsa02: usize,
}

static SCHEMA: OnceLock<Result<Schema, String>> = OnceLock::new();

fn schema() -> Result<&'static Schema, String> {
    SCHEMA
        .get_or_init(|| Schema::load(include_str!("provenance-schema.json")))
        .as_ref()
        .map_err(Clone::clone)
}

/// foldName for a key: ASCII letters upper-cased; `ſ` and the Kelvin sign fold to `S`
/// and `K`, the only others whose fold is ASCII (measured over every rune with Go's
/// unicode.SimpleFold), so a key with any other non-ASCII rune matches no field.
fn fold(key: &str) -> Option<String> {
    key.chars()
        .map(|c| match c {
            c if c.is_ascii() => Some(c.to_ascii_uppercase()),
            '\u{17F}' => Some('S'),
            '\u{212A}' => Some('K'),
            _ => None,
        })
        .collect()
}

impl Schema {
    fn load(text: &str) -> Result<Schema, String> {
        let bad = |what: &str| format!("provenance schema: {what}");
        let doc: serde_json::Value = serde_json::from_str(text).map_err(|e| bad(&e.to_string()))?;
        let types = doc
            .get("types")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| bad("no types"))?;
        let ids: Vec<&String> = types.keys().collect();
        let index = |id: &str| -> Result<usize, String> {
            ids.iter()
                .position(|x| x.as_str() == id)
                .ok_or_else(|| bad(&format!("no type {id}")))
        };
        let custom = doc.get("custom").and_then(serde_json::Value::as_object);
        let str_of =
            |v: &serde_json::Value, k: &str| v.get(k).and_then(serde_json::Value::as_str).map(str::to_string);
        let fields_of = |v: &serde_json::Value| -> Result<Vec<Field>, String> {
            let mut out = Vec::new();
            for f in v
                .get("fields")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = str_of(f, "name").ok_or_else(|| bad("field without a name"))?;
                if !name.is_ascii() {
                    return Err(bad(&format!("field {name} is not ASCII")));
                }
                if f.get("quoted").is_some() || f.get("unsettable").is_some() {
                    return Err(bad(&format!(
                        "field {name} needs ,string or an unsettable pointer"
                    )));
                }
                let ty = index(&str_of(f, "type").ok_or_else(|| bad("field without a type"))?)?;
                out.push(Field {
                    folded: name.to_ascii_uppercase(),
                    name,
                    ty,
                });
            }
            Ok(out)
        };
        let mut out = Vec::with_capacity(ids.len());
        for (id, v) in types {
            let kind = str_of(v, "kind").unwrap_or_default();
            let bits = || {
                v.get("bits")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|b| u32::try_from(b).ok())
                    .ok_or_else(|| bad(&format!("{id} without bits")))
            };
            let elem = || index(&str_of(v, "elem").unwrap_or_default());
            out.push(match kind.as_str() {
                "string" => Ty::Str,
                "bool" => Ty::Bool,
                "int" => Ty::Int(bits()?),
                "uint" => Ty::Uint(bits()?),
                "float" => Ty::Float(bits()?),
                "any" => Ty::Any,
                "iface" => Ty::Iface,
                "ptr" => Ty::Ptr(elem()?),
                "slice" => Ty::Slice(elem()?),
                "map" => {
                    let key = match str_of(v, "key").as_deref() {
                        Some("string") => MapKey::Str,
                        Some(k) if k.starts_with("int") => {
                            MapKey::Int(k.get(3..).and_then(|b| b.parse().ok()).ok_or_else(|| bad(k))?)
                        }
                        Some(k) if k.starts_with("uint") => {
                            MapKey::Uint(k.get(4..).and_then(|b| b.parse().ok()).ok_or_else(|| bad(k))?)
                        }
                        _ => return Err(bad(&format!("{id}: map key"))),
                    };
                    Ty::Map(key, elem()?)
                }
                "struct" => Ty::Struct(fields_of(v)?),
                "custom" => match id.as_str() {
                    "time.Time" => Ty::Time,
                    "encoding/json.RawMessage" => Ty::Raw,
                    "github.com/moby/buildkit/solver/llbsolver/provenance/types.Environment"
                    | "github.com/moby/buildkit/solver/llbsolver/provenance/types.ProvenanceInternalParametersSLSA1" => {
                        Ty::Flatten(fields_of(v)?)
                    }
                    _ => {
                        let via = custom
                            .and_then(|c| c.get(id.as_str()))
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| bad(&format!("{id}: an UnmarshalJSON shards does not know")))?;
                        Ty::Via(index(via)?)
                    }
                },
                other => return Err(bad(&format!("{id}: kind {other}"))),
            });
        }
        let root = |k: &str| {
            doc.get("roots")
                .and_then(|r| r.get(k))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad(&format!("no root {k}")))
                .and_then(index)
        };
        Ok(Schema {
            statement: root("statement")?,
            slsa1: root("slsa1")?,
            slsa02: root("slsa02")?,
            types: out,
        })
    }

    /// The fields of a struct, through pointers.
    fn fields(&self, mut ty: usize) -> &[Field] {
        loop {
            match self.types.get(ty) {
                Some(Ty::Ptr(e)) => ty = *e,
                Some(Ty::Struct(f) | Ty::Flatten(f)) => return f,
                _ => return &[],
            }
        }
    }

    /// Whether `json.Unmarshal` of the value into the type succeeds.
    fn check(&self, doc: &Doc<'_>, n: usize, ty: usize) -> bool {
        // `None`: any JSON, its numbers float64s.
        let mut work: Vec<(usize, Option<usize>)> = vec![(n, Some(ty))];
        while let Some((n, ty)) = work.pop() {
            let Some(node) = doc.node(n) else {
                return false;
            };
            let Some(ty) = ty else {
                match &node.kind {
                    Kind::Number if !float_fits(doc.number(n), 64) => return false,
                    Kind::Arr(items) => work.extend(items.iter().map(|&c| (c, None))),
                    Kind::Obj(members) => work.extend(members.iter().map(|&(_, c)| (c, None))),
                    _ => {}
                }
                continue;
            };
            if matches!(node.kind, Kind::Null) {
                continue;
            }
            let Some(t) = self.types.get(ty) else {
                return false;
            };
            let ok = match (t, &node.kind) {
                (Ty::Str, Kind::Str(_)) | (Ty::Bool, Kind::Bool(_)) | (Ty::Raw, _) => true,
                (Ty::Int(bits), Kind::Number) => int_fits(doc.number(n), *bits),
                (Ty::Uint(bits), Kind::Number) => uint_fits(doc.number(n), *bits),
                (Ty::Float(bits), Kind::Number) => float_fits(doc.number(n), *bits),
                (Ty::Any, _) => {
                    work.push((n, None));
                    true
                }
                (Ty::Ptr(e) | Ty::Via(e), _) => {
                    work.push((n, Some(*e)));
                    true
                }
                (Ty::Slice(e), Kind::Arr(items)) => {
                    work.extend(items.iter().map(|&c| (c, Some(*e))));
                    true
                }
                // A []byte from base64.
                (Ty::Slice(e), Kind::Str(s)) => {
                    matches!(self.types.get(*e), Some(Ty::Uint(8)))
                        && shards_sigstore::gobase64::decode(s.as_bytes(), false, true).is_ok()
                }
                (Ty::Map(key, e), Kind::Obj(members)) => {
                    for (k, c) in members {
                        let fits = match key {
                            MapKey::Str => true,
                            MapKey::Int(bits) => int_fits(k, *bits),
                            MapKey::Uint(bits) => uint_fits(k, *bits),
                        };
                        if !fits {
                            return false;
                        }
                        work.push((*c, Some(*e)));
                    }
                    true
                }
                (Ty::Struct(fields), Kind::Obj(members)) => {
                    work.extend(
                        members
                            .iter()
                            .filter_map(|(k, c)| field(fields, k).map(|f| (*c, Some(f.ty)))),
                    );
                    true
                }
                (Ty::Flatten(fields), Kind::Obj(members)) => {
                    work.push((n, None));
                    work.extend(
                        members
                            .iter()
                            .filter_map(|(k, c)| field(fields, k).map(|f| (*c, Some(f.ty)))),
                    );
                    true
                }
                (Ty::Time, Kind::Str(_)) => shards_dockerfile::go::parse_rfc3339(doc.raw_string(n)).is_ok(),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// An object's members as decoding into the struct `ty` meets them: each member's
    /// field, its value and the field's type; members of no field left out. None for
    /// anything but an object (null leaves a struct as it was).
    fn members<'s>(&'s self, doc: &Doc<'_>, n: usize, ty: usize) -> Vec<(&'s str, usize, usize)> {
        let fields = self.fields(ty);
        match doc.node(n).map(|x| &x.kind) {
            Some(Kind::Obj(members)) => members
                .iter()
                .filter_map(|(k, c)| field(fields, k).map(|f| (f.name.as_str(), *c, f.ty)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// The field a key decodes into: by its exact name, else the first whose folded name is
/// the key's.
fn field<'f>(fields: &'f [Field], key: &str) -> Option<&'f Field> {
    fields.iter().find(|f| f.name == key).or_else(|| {
        let k = fold(key)?;
        fields.iter().find(|f| f.folded == k)
    })
}

/// strconv.ParseInt(text, 10, 64), then the field's width.
fn int_fits(text: &str, bits: u32) -> bool {
    let Ok(v) = text.parse::<i64>() else {
        return false;
    };
    bits >= 64 || {
        let half = 1i128 << (bits - 1);
        (-half..half).contains(&i128::from(v))
    }
}

/// strconv.ParseUint(text, 10, 64), then the field's width.
fn uint_fits(text: &str, bits: u32) -> bool {
    let Ok(v) = text.parse::<u64>() else {
        return false;
    };
    bits >= 64 || u128::from(v) < (1u128 << bits)
}

/// strconv.ParseFloat(text, bits): a number past the largest is out of range.
fn float_fits(text: &str, bits: u32) -> bool {
    if bits == 32 {
        text.parse::<f32>().is_ok_and(f32::is_finite)
    } else {
        text.parse::<f64>().is_ok_and(f64::is_finite)
    }
}

// ---- decoding the fields buildx reads ----

/// A string field: a string sets it, null leaves it.
fn set_str(doc: &Doc<'_>, n: usize, s: &mut String) {
    if let Some(Kind::Str(v)) = doc.node(n).map(|x| &x.kind) {
        s.clone_from(v);
    }
}

fn set_bool(doc: &Doc<'_>, n: usize, b: &mut bool) {
    if let Some(Kind::Bool(v)) = doc.node(n).map(|x| &x.kind) {
        *b = *v;
    }
}

/// A `map[string]string`: null clears it; an object adds to it, a null value as "".
fn set_map(doc: &Doc<'_>, n: usize, m: &mut Option<BTreeMap<String, String>>) {
    match doc.node(n).map(|x| &x.kind) {
        Some(Kind::Null) => *m = None,
        Some(Kind::Obj(members)) => {
            let m = m.get_or_insert_default();
            for (k, c) in members {
                let mut v = String::new();
                set_str(doc, *c, &mut v);
                m.insert(k.clone(), v);
            }
        }
        _ => {}
    }
}

/// A `*time.Time`: null clears it, a string replaces it.
fn set_time(doc: &Doc<'_>, n: usize, t: &mut Option<shards_dockerfile::go::Time>) {
    match doc.node(n).map(|x| &x.kind) {
        Some(Kind::Null) => *t = None,
        Some(Kind::Str(_)) => *t = shards_dockerfile::go::parse_rfc3339(doc.raw_string(n)).ok(),
        _ => {}
    }
}

/// A slice of structs as Go's decoder keeps one: elements past its length stay in its
/// backing array, and an array decodes into them again.
#[derive(Debug, Default)]
struct GoSlice<T> {
    items: Vec<T>,
    len: usize,
}

impl<T: Default> GoSlice<T> {
    fn decode(&mut self, doc: &Doc<'_>, n: usize, mut each: impl FnMut(&mut T, usize)) {
        match doc.node(n).map(|x| &x.kind) {
            Some(Kind::Null) => {
                self.items.clear();
                self.len = 0;
            }
            Some(Kind::Arr(elements)) => {
                for (i, &c) in elements.iter().enumerate() {
                    if i >= self.items.len() {
                        self.items.push(T::default());
                    }
                    if let Some(slot) = self.items.get_mut(i) {
                        each(slot, c);
                    }
                }
                self.len = elements.len();
                if elements.is_empty() {
                    self.items.clear();
                }
            }
            _ => {}
        }
    }

    fn live(&self) -> &[T] {
        self.items.get(..self.len).unwrap_or_default()
    }
}

#[derive(Debug, Default)]
struct Config {
    uri: String,
    digest: Option<BTreeMap<String, String>>,
    path: String,
}

#[derive(Debug, Default)]
struct Dep {
    uri: String,
    digest: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default)]
struct Metadata {
    invocation_id: String,
    started: Option<shards_dockerfile::go::Time>,
    finished: Option<shards_dockerfile::go::Time>,
    hermetic: bool,
    reproducible: bool,
    parameters: bool,
    environment: bool,
    materials: bool,
}

/// What buildx reads of a ProvenancePredicateSLSA1.
#[derive(Debug, Default)]
struct V1 {
    build_type: String,
    builder_id: String,
    config: Config,
    frontend: String,
    args: Option<BTreeMap<String, String>>,
    deps: GoSlice<Dep>,
    meta: Option<Metadata>,
}

/// What buildx reads of a ProvenancePredicateSLSA02.
#[derive(Debug, Default)]
struct V02 {
    build_type: String,
    builder_id: String,
    config: Config,
    frontend: String,
    args: Option<BTreeMap<String, String>>,
    materials: GoSlice<Dep>,
    meta: Option<Metadata>,
}

impl Schema {
    fn dep(&self, doc: &Doc<'_>, n: usize, ty: usize, d: &mut Dep) {
        for (name, c, _) in self.members(doc, n, ty) {
            match name {
                "uri" => set_str(doc, c, &mut d.uri),
                "digest" => set_map(doc, c, &mut d.digest),
                _ => {}
            }
        }
    }

    /// A struct with `uri`, `digest` and the path's field, `path_field`.
    fn config(&self, doc: &Doc<'_>, n: usize, ty: usize, path_field: &str, c: &mut Config) {
        for (name, v, _) in self.members(doc, n, ty) {
            match name {
                "uri" => set_str(doc, v, &mut c.uri),
                "digest" => set_map(doc, v, &mut c.digest),
                p if p == path_field => set_str(doc, v, &mut c.path),
                _ => {}
            }
        }
    }

    /// `Parameters`: its frontend and arguments.
    fn parameters(
        &self,
        doc: &Doc<'_>,
        n: usize,
        ty: usize,
        frontend: &mut String,
        args: &mut Option<BTreeMap<String, String>>,
    ) {
        for (name, v, _) in self.members(doc, n, ty) {
            match name {
                "frontend" => set_str(doc, v, frontend),
                "args" => set_map(doc, v, args),
                _ => {}
            }
        }
    }

    /// A pointer to a struct: null clears it, an object decodes into it, made where it
    /// was nil.
    fn pointer<T: Default>(&self, doc: &Doc<'_>, n: usize, p: &mut Option<T>, f: impl FnOnce(&mut T)) {
        match doc.node(n).map(|x| &x.kind) {
            Some(Kind::Null) => *p = None,
            Some(Kind::Obj(_)) => f(p.get_or_insert_default()),
            _ => {}
        }
    }

    fn v1(&self, doc: &Doc<'_>, n: usize) -> V1 {
        let mut p = V1::default();
        for (name, c, t) in self.members(doc, n, self.slsa1) {
            match name {
                "buildDefinition" => {
                    for (name, c, t) in self.members(doc, c, t) {
                        match name {
                            "buildType" => set_str(doc, c, &mut p.build_type),
                            "externalParameters" => {
                                for (name, c, t) in self.members(doc, c, t) {
                                    match name {
                                        "configSource" => self.config(doc, c, t, "path", &mut p.config),
                                        "request" => self.parameters(doc, c, t, &mut p.frontend, &mut p.args),
                                        _ => {}
                                    }
                                }
                            }
                            "resolvedDependencies" => {
                                let elem = self.elem(t);
                                p.deps.decode(doc, c, |d, c| self.dep(doc, c, elem, d));
                            }
                            _ => {}
                        }
                    }
                }
                "runDetails" => {
                    for (name, c, t) in self.members(doc, c, t) {
                        match name {
                            "builder" => {
                                for (name, c, _) in self.members(doc, c, t) {
                                    if name == "id" {
                                        set_str(doc, c, &mut p.builder_id);
                                    }
                                }
                            }
                            "metadata" => self.pointer(doc, c, &mut p.meta, |m| {
                                for (name, c, t) in self.members(doc, c, t) {
                                    match name {
                                        "invocationId" => set_str(doc, c, &mut m.invocation_id),
                                        "startedOn" => set_time(doc, c, &mut m.started),
                                        "finishedOn" => set_time(doc, c, &mut m.finished),
                                        "buildkit_hermetic" => set_bool(doc, c, &mut m.hermetic),
                                        "buildkit_reproducible" => set_bool(doc, c, &mut m.reproducible),
                                        "buildkit_completeness" => {
                                            for (name, c, _) in self.members(doc, c, t) {
                                                match name {
                                                    "request" => set_bool(doc, c, &mut m.parameters),
                                                    "resolvedDependencies" => {
                                                        set_bool(doc, c, &mut m.materials)
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        p
    }

    fn v02(&self, doc: &Doc<'_>, n: usize) -> V02 {
        let mut p = V02::default();
        for (name, c, t) in self.members(doc, n, self.slsa02) {
            match name {
                "builder" => {
                    for (name, c, _) in self.members(doc, c, t) {
                        if name == "id" {
                            set_str(doc, c, &mut p.builder_id);
                        }
                    }
                }
                "buildType" => set_str(doc, c, &mut p.build_type),
                "invocation" => {
                    for (name, c, t) in self.members(doc, c, t) {
                        match name {
                            "configSource" => self.config(doc, c, t, "entryPoint", &mut p.config),
                            "parameters" => self.parameters(doc, c, t, &mut p.frontend, &mut p.args),
                            _ => {}
                        }
                    }
                }
                "materials" => {
                    let elem = self.elem(t);
                    p.materials.decode(doc, c, |d, c| self.dep(doc, c, elem, d));
                }
                "metadata" => self.pointer(doc, c, &mut p.meta, |m| {
                    for (name, c, t) in self.members(doc, c, t) {
                        match name {
                            "buildInvocationID" => set_str(doc, c, &mut m.invocation_id),
                            "buildStartedOn" => set_time(doc, c, &mut m.started),
                            "buildFinishedOn" => set_time(doc, c, &mut m.finished),
                            "reproducible" => set_bool(doc, c, &mut m.reproducible),
                            "https://mobyproject.org/buildkit@v1#hermetic" => {
                                set_bool(doc, c, &mut m.hermetic)
                            }
                            "completeness" => {
                                for (name, c, _) in self.members(doc, c, t) {
                                    match name {
                                        "parameters" => set_bool(doc, c, &mut m.parameters),
                                        "environment" => set_bool(doc, c, &mut m.environment),
                                        "materials" => set_bool(doc, c, &mut m.materials),
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }),
                _ => {}
            }
        }
        p
    }

    /// A slice's element type.
    fn elem(&self, ty: usize) -> usize {
        match self.types.get(ty) {
            Some(Ty::Slice(e)) => *e,
            _ => ty,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value as J, json};

    use super::*;
    use crate::build::policy::{GitMeta, ImageMeta};

    /// Cases where shards does not do what buildx does, and why.
    const DEVIATIONS: &[(&str, &str)] = &[(
        "material purl qualifier without value",
        "packageurl-go reads kv[1] of a qualifier without `=` and panics, ending buildx; \
         shards skips the material as one it cannot read",
    )];

    fn data() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/policy")
    }

    fn text(v: &J, k: &str) -> String {
        v.get(k).and_then(J::as_str).unwrap_or_default().to_string()
    }

    fn b64(s: &str) -> Vec<u8> {
        shards_sigstore::gobase64::decode(s.as_bytes(), false, true).unwrap()
    }

    fn platform_of(v: Option<&J>) -> Option<Platform> {
        let v = v.filter(|v| !v.is_null())?;
        Some(Platform {
            os: text(v, "os").into_bytes(),
            architecture: text(v, "arch").into_bytes(),
            variant: text(v, "variant").into_bytes(),
            ..Platform::default()
        })
    }

    fn image_of(v: &J) -> ImageMeta {
        let chain = v.get("chain").filter(|c| !c.is_null()).map(|c| {
            let mut blobs = BTreeMap::new();
            for b in c.get("blobs").and_then(J::as_array).into_iter().flatten() {
                // shards' chains always describe their blobs, as BuildKit's do: a blob
                // without a descriptor is one buildx skips, as it is one never read.
                if b.get("noDescriptor").and_then(J::as_bool) == Some(true) {
                    continue;
                }
                let data = match b.get("dataFile").and_then(J::as_str) {
                    Some(f) => std::fs::read(data().join(f)).unwrap(),
                    None => b64(&text(b, "data")),
                };
                let annotations = b
                    .get("annotations")
                    .and_then(J::as_object)
                    .map(|a| {
                        a.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let desc = shards_sigstore::image::Descriptor {
                    media_type: text(b, "mediaType"),
                    digest: text(b, "key"),
                    size: i64::try_from(data.len()).unwrap(),
                    annotations,
                    ..Default::default()
                };
                blobs.insert(text(b, "key"), (desc, data));
            }
            AttestationChain {
                attestation_manifest: text(c, "attestationManifest"),
                blobs,
                ..AttestationChain::default()
            }
        });
        ImageMeta {
            digest: text(v, "digest"),
            config: v.get("config").and_then(J::as_str).map(b64),
            attestation_chain: chain,
        }
    }

    fn level(l: LogLevel) -> &'static str {
        match l {
            LogLevel::Warn => "warning",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            _ => "other",
        }
    }

    /// A request as the oracle records one: a call's platform is the image's only.
    fn record(source: &str, r: &MetaRequest, call: bool) -> J {
        let mut m = Map::new();
        m.insert("source".into(), json!(source));
        let platform = if call && r.image.is_none() {
            None
        } else {
            r.platform.as_ref()
        };
        if let Some(p) = platform {
            let mut pm = Map::new();
            pm.insert("os".into(), json!(String::from_utf8_lossy(&p.os)));
            pm.insert("arch".into(), json!(String::from_utf8_lossy(&p.architecture)));
            if !p.variant.is_empty() {
                pm.insert("variant".into(), json!(String::from_utf8_lossy(&p.variant)));
            }
            m.insert("platform".into(), J::Object(pm));
        }
        if let Some(i) = &r.image {
            m.insert("image".into(), json!(true));
            if i.no_config {
                m.insert("noConfig".into(), json!(true));
            }
            if i.attestation_chain {
                m.insert("attestationChain".into(), json!(true));
            }
            if !i.resolve_attestations.is_empty() {
                m.insert("resolveAttestations".into(), json!(i.resolve_attestations));
            }
        }
        if let Some(g) = &r.git {
            m.insert("git".into(), json!(true));
            if g.return_object {
                m.insert("returnObject".into(), json!(true));
            }
        }
        if r.http_checksum.is_some() {
            m.insert("http".into(), json!(true));
        }
        J::Object(m)
    }

    /// The oracle's resolver: its replies by identifier, a git source's ref and checksum.
    struct Fake<'a> {
        replies: Option<&'a Map<String, J>>,
        calls: RefCell<Vec<J>>,
    }

    impl Resolve for Fake<'_> {
        fn resolve(&self, source: &Source, request: &MetaRequest) -> Result<Meta, String> {
            self.calls
                .borrow_mut()
                .push(record(&source.identifier, request, true));
            let Some(reply) = self.replies.and_then(|r| r.get(&source.identifier)) else {
                return Err(format!("no reply for {}", source.identifier));
            };
            if let Some(e) = reply.get("error").and_then(J::as_str) {
                return Err(e.to_string());
            }
            let mut meta = Meta {
                image: reply.get("image").filter(|i| !i.is_null()).map(image_of),
                ..Meta::default()
            };
            if source.identifier.starts_with("git://") {
                meta.git = Some(GitMeta {
                    reference: "refs/heads/main".into(),
                    checksum: "1111111111111111111111111111111111111111".into(),
                    ..GitMeta::default()
                });
            }
            Ok(meta)
        }
    }

    fn strings(v: Option<&J>) -> Vec<String> {
        v.and_then(J::as_array)
            .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn provenance_reads_as_buildx_reads_it() {
        let cases: Vec<J> =
            serde_json::from_slice(&std::fs::read(data().join("provenance.json")).unwrap()).unwrap();
        let mut failures = Vec::new();
        let mut compared = 0;
        for c in &cases {
            let name = text(c, "name");
            let meta_case = c.get("meta").unwrap();
            let src = meta_case.get("source").unwrap();
            let mut source = Source::new(text(src, "identifier"));
            if let Some(a) = src.get("attrs").and_then(J::as_object) {
                source.attrs = a
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect();
            }
            let meta = Meta {
                image: meta_case.get("image").filter(|i| !i.is_null()).map(image_of),
                ..Meta::default()
            };
            let platform = platform_of(c.get("platform"));
            let logs = RefCell::new(Vec::new());
            let mut log = |l: LogLevel, t: &str| logs.borrow_mut().push(format!("{}: {t}", level(l)));
            let got = source_to_input(&source, &meta, platform.as_ref(), None, &mut log);
            if let Some((_, why)) = DEVIATIONS.iter().find(|(n, _)| *n == name) {
                assert!(text(c, "error").starts_with("panic:"), "{name}: {why}");
                assert!(got.is_ok(), "{name}");
                assert!(
                    logs.borrow().iter().any(|l| l.contains("no value")),
                    "{name}: {:?}",
                    logs.borrow()
                );
                continue;
            }
            compared += 1;
            let mut problems = Vec::new();
            let want_logs = strings(c.get("logs"));
            let mut inp = match got {
                Err(e) => {
                    if e != text(c, "error") {
                        problems.push(format!("error: got {e:?}, want {:?}", text(c, "error")));
                    }
                    if !problems.is_empty() {
                        failures.push((name, problems));
                    }
                    continue;
                }
                Ok(inp) => inp,
            };
            if !text(c, "error").is_empty() {
                problems.push(format!("want error {:?}", text(c, "error")));
            }
            let json = inp.json().compact();
            if json != text(c, "input") {
                problems.push(format!("input:\n got  {json}\n want {}", text(c, "input")));
            }
            if inp.unknown_refs() != strings(c.get("unknowns")) {
                problems.push(format!(
                    "unknowns: got {:?}, want {:?}",
                    inp.unknown_refs(),
                    strings(c.get("unknowns"))
                ));
            }
            if *logs.borrow() != want_logs {
                problems.push(format!("logs: got {:?}, want {want_logs:?}", logs.borrow()));
            }
            if let (Some(r), Some(want)) = (c.get("resolve"), c.get("resolved")) {
                logs.borrow_mut().clear();
                let fake = Fake {
                    replies: r.get("replies").and_then(J::as_object),
                    calls: RefCell::new(Vec::new()),
                };
                let unknowns = strings(r.get("unknowns"));
                let got = resolve_input_unknowns(
                    &mut inp,
                    &source,
                    &unknowns,
                    platform.as_ref(),
                    platform.as_ref(),
                    Some(&fake),
                    None,
                    &mut log,
                );
                let (retry, next, error) = match got {
                    Ok((retry, next)) => (retry, next, String::new()),
                    Err(e) => (false, None, e),
                };
                let want_retry = want.get("retry").and_then(J::as_bool).unwrap_or(false);
                if retry != want_retry || error != text(want, "error") {
                    problems.push(format!(
                        "resolve: got retry {retry} error {error:?}, want {want_retry} {:?}",
                        text(want, "error")
                    ));
                }
                let next = next.map(|n| record(&source.identifier, &n, false));
                if next.as_ref() != want.get("next") {
                    problems.push(format!("next: got {next:?}, want {:?}", want.get("next")));
                }
                let calls = fake.calls.borrow().clone();
                let want_calls = want
                    .get("calls")
                    .and_then(J::as_array)
                    .cloned()
                    .unwrap_or_default();
                if calls != want_calls {
                    problems.push(format!("calls: got {calls:?}, want {want_calls:?}"));
                }
                let json = inp.json().compact();
                if json != text(want, "input") {
                    problems.push(format!(
                        "resolved input:\n got  {json}\n want {}",
                        text(want, "input")
                    ));
                }
                if inp.unknown_refs() != strings(want.get("unknowns")) {
                    problems.push(format!(
                        "resolved unknowns: got {:?}, want {:?}",
                        inp.unknown_refs(),
                        strings(want.get("unknowns"))
                    ));
                }
                if *logs.borrow() != strings(want.get("logs")) {
                    problems.push(format!(
                        "resolve logs: got {:?}, want {:?}",
                        logs.borrow(),
                        strings(want.get("logs"))
                    ));
                }
            }
            if !problems.is_empty() {
                failures.push((name, problems));
            }
        }
        assert!(compared > 200, "{compared} cases");
        assert!(
            failures.is_empty(),
            "{} of {compared} differ:\n{}",
            failures.len(),
            failures
                .iter()
                .map(|(n, p)| format!("== {n}\n{}", p.join("\n")))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn the_schema_loads() {
        let s = schema().unwrap();
        assert!(matches!(s.types.get(s.slsa1), Some(Ty::Struct(_))));
        assert!(matches!(s.types.get(s.slsa02), Some(Ty::Struct(_))));
    }

    /// Paths SourceToInput cannot reach, a material never having metadata of its own:
    /// the depth limit and a cycle.
    #[test]
    fn materials_stop_at_the_depth_limit_and_at_a_cycle() {
        let source = Source::new("docker-image://docker.io/library/alpine:3.20");
        let platform = Platform::new("linux", "arm64");
        let mut log = |_: LogLevel, _: &str| {};
        let deep = recursive(
            &source,
            &Meta::default(),
            Some(&platform),
            25,
            &mut Vec::new(),
            None,
            &mut log,
        );
        assert_eq!(deep.unwrap_err(), "provenance materials depth exceeds limit 24");
        let at_limit = recursive(
            &source,
            &Meta::default(),
            Some(&platform),
            24,
            &mut Vec::new(),
            None,
            &mut log,
        );
        assert_eq!(at_limit.unwrap().env.depth, 24);
        let real = std::fs::read(data().join("real/buildkit-v0.28.1-arm64.provenance.json")).unwrap();
        let mut blobs = BTreeMap::new();
        let mut desc = shards_sigstore::image::Descriptor::default();
        desc.annotations.insert(PREDICATE_TYPE.into(), SLSA_V1.into());
        blobs.insert("sha256:1".to_string(), (desc, real));
        let meta = Meta {
            image: Some(ImageMeta {
                digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                config: None,
                attestation_chain: Some(AttestationChain {
                    blobs,
                    ..AttestationChain::default()
                }),
            }),
            ..Meta::default()
        };
        let mut seen = vec![unique_identifier(&source, Some(&platform))];
        let cycle = recursive(&source, &meta, Some(&platform), 1, &mut seen, None, &mut log).unwrap();
        assert_eq!(cycle.json().compact(), "{}");
        let fresh = recursive(
            &source,
            &meta,
            Some(&platform),
            0,
            &mut Vec::new(),
            None,
            &mut log,
        )
        .unwrap();
        let materials = &fresh.image.unwrap().provenance.unwrap().materials;
        assert_eq!(materials.len(), 10);
    }
}
