//! SBOM attestations as BuildKit v0.28.1 makes them (D81): a scanner image, the generator,
//! run over the build's result (frontend/attestations/sbom): its command with the result
//! mounted read-only at `/run/src/core/sbom`, each extra target (a stage, the context) at
//! `/run/src/extras/sbom-<name>`, a tmpfs at `/tmp`, and what it writes in `/run/out`, one
//! in-toto statement of an SPDX document a file, attached to the image beside its
//! provenance (exporter/attestation).

use std::collections::BTreeMap;

use shards_build::data::Sources;
use shards_dockerfile::image::Image;
use shards_dockerfile::llb::{EnvList, Graph, Meta, Mount, MountKind, Run, State};
use shards_dockerfile::platform::Platform;
use shards_image::erofs::{Kind, Source as _, Tree};

/// The predicate type an SBOM's statement has (in-toto's `PredicateSPDX`).
pub const PREDICATE: &str = "https://spdx.dev/Document";
/// The scanner BuildKit runs unless another is named (`attestations.Parse`).
pub const DEFAULT_GENERATOR: &str = "docker/buildkit-syft-scanner:stable-1";
/// The core target's name: the build's result.
const CORE: &str = "sbom";
const SRC: &str = "/run/src/";
const OUT: &str = "/run/out/";

/// An SBOM asked for (`--sbom`, `--attest type=sbom`): the generator, and the parameters
/// it is given as `BUILDKIT_SCAN_<KEY>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub generator: String,
    pub params: BTreeMap<String, String>,
}

/// `CreateSBOMScanner`'s scan: the generator `scanner` (its reference, `pin` the digest it
/// resolved to, and its config) run on `platform` over `core` and `extras`, as a step of
/// `graph` named for `id`; the state of its output, `/run/out`.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    graph: &mut Graph,
    scanner: &[u8],
    pin: &[u8],
    config: &Image,
    platform: &Platform,
    id: &str,
    core: &State,
    extras: &[(Vec<u8>, State)],
    params: &BTreeMap<String, String>,
) -> Result<State, String> {
    let mut args = config.config.entrypoint.clone();
    args.extend(config.config.cmd.iter().cloned());
    if args.is_empty() {
        return Err(format!(
            "scanner {} does not have cmd",
            String::from_utf8_lossy(scanner)
        ));
    }
    // Pinned to what it resolved to, as a stage's base is; shown by its identifier, as
    // BuildKit names a source of no name of its own.
    let identifier = if scanner.contains(&b'@') || pin.is_empty() {
        [b"docker-image://".as_slice(), scanner].concat()
    } else {
        [b"docker-image://".as_slice(), scanner, b"@", pin].concat()
    };
    let mut shown = Meta::default();
    shown
        .description
        .insert(b"llb.customname".to_vec(), identifier.clone());
    let mut source = graph.source(identifier, BTreeMap::new(), Some(platform.clone()), shown);
    let mut env = EnvList::new();
    for e in &config.config.env {
        let (k, v) = match e.iter().position(|&c| c == b'=') {
            Some(at) => (
                e.get(..at).unwrap_or_default(),
                e.get(at + 1..).unwrap_or_default(),
            ),
            None => (e.as_slice(), &[][..]),
        };
        env.add(k, v);
    }
    env.add(b"BUILDKIT_SCAN_DESTINATION", OUT.as_bytes());
    env.add(b"BUILDKIT_SCAN_SOURCE", format!("{SRC}core/{CORE}").as_bytes());
    if !extras.is_empty() {
        env.add(b"BUILDKIT_SCAN_SOURCE_EXTRAS", format!("{SRC}extras/").as_bytes());
    }
    for (k, v) in params {
        env.add(format!("BUILDKIT_SCAN_{k}").as_bytes(), v.as_bytes());
    }
    source.env = env;
    source.set_dir(&config.config.working_dir);
    let mut mounts = vec![
        Mount {
            target: b"/tmp".to_vec(),
            source: None,
            readonly: false,
            selector: Vec::new(),
            kind: MountKind::Tmpfs { size: 0 },
            no_output: false,
        },
        Mount {
            target: format!("{SRC}core/{CORE}").into_bytes(),
            source: core.output,
            readonly: true,
            selector: Vec::new(),
            kind: MountKind::Bind,
            no_output: false,
        },
    ];
    for (name, st) in extras {
        mounts.push(Mount {
            target: [format!("{SRC}extras/{CORE}-").as_bytes(), name].concat(),
            source: st.output,
            readonly: true,
            selector: Vec::new(),
            kind: MountKind::Bind,
            no_output: false,
        });
    }
    mounts.push(Mount {
        target: OUT.as_bytes().to_vec(),
        source: None,
        readonly: false,
        selector: Vec::new(),
        kind: MountKind::Bind,
        no_output: false,
    });
    let mut meta = Meta::default();
    meta.description.insert(
        b"llb.customname".to_vec(),
        format!(
            "[{id}] generating sbom using {}",
            String::from_utf8_lossy(scanner)
        )
        .into_bytes(),
    );
    let root = graph.run(
        &source,
        Run {
            args,
            mounts,
            meta,
            ..Run::default()
        },
    );
    // The output of `/run/out`: after the root's, the one other writable bind mount, as
    // the mounts are sorted by target.
    let Some(mut out) = root.output else {
        return Err("the scan has no output".into());
    };
    out.index = 1;
    Ok(State {
        output: Some(out),
        ..State::scratch()
    })
}

/// A statement the scanner wrote: its file's name, its predicate type, its predicate as
/// Go writes a raw message (compact, HTML-escaped), and its own subjects, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    pub path: String,
    pub predicate_type: String,
    pub predicate: Vec<u8>,
    pub subjects: Vec<(String, BTreeMap<String, String>)>,
}

/// `unbundle`: each file of the scan's output, by name, read as one in-toto statement of
/// an SPDX document; then `sort`, the core target's first. A file of more than `max`
/// bytes is refused.
pub fn read(tree: &Tree, sources: &mut Sources, max: u64) -> Result<Vec<Scanned>, String> {
    let mut out = Vec::new();
    for (name, id) in tree.entries(Tree::ROOT) {
        let shown = String::from_utf8_lossy(name).into_owned();
        let Some(node) = tree.node(id) else { continue };
        let Kind::File { size, data } = &node.kind else {
            return Err(format!("cannot decode in-toto statement: {shown} is not a file"));
        };
        if *size > max {
            return Err(format!("in-toto statement {shown} is larger than {max} bytes"));
        }
        let len = usize::try_from(*size).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; len];
        sources.read_at(*data, 0, &mut buf).map_err(|e| e.to_string())?;
        out.push(statement(&shown, &buf)?);
    }
    // The core target's first: a file whose name, up to its first dot, is the core's.
    let (mut core, rest): (Vec<Scanned>, Vec<Scanned>) = out
        .into_iter()
        .partition(|s| s.path.split('.').next() == Some(CORE));
    core.extend(rest);
    Ok(core)
}

/// One statement file, as `unbundle` reads it.
pub fn statement(path: &str, text: &[u8]) -> Result<Scanned, String> {
    let v: serde_json::Value =
        serde_json::from_slice(text).map_err(|e| format!("cannot decode in-toto statement: {e}"))?;
    if !v.is_object() {
        return Err("in-toto statement is not a single JSON object".into());
    }
    let field = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);
    let predicate_type = field("predicateType").as_str().unwrap_or_default().to_string();
    if predicate_type != PREDICATE {
        return Err(format!(
            "bundle entry {predicate_type} does not match required predicate type {PREDICATE}"
        ));
    }
    let mut subjects = Vec::new();
    for s in field("subject").as_array().into_iter().flatten() {
        let digest = s
            .get("digest")
            .and_then(serde_json::Value::as_object)
            .map(|d| {
                d.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let name = s
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        subjects.push((name.to_string(), digest));
    }
    let compact = shards_dockerfile::json_compact(text)?;
    let predicate = member(&compact, b"predicate")
        .map(escape_html)
        .unwrap_or_else(|| b"null".to_vec());
    Ok(Scanned {
        path: path.to_string(),
        predicate_type,
        predicate,
        subjects,
    })
}

/// The value of the top-level object member `key` in compact JSON `text`.
fn member<'t>(text: &'t [u8], key: &[u8]) -> Option<&'t [u8]> {
    let want = [b"\"".as_slice(), key, b"\":"].concat();
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    let mut at = 0;
    while at < text.len() {
        let b = *text.get(at)?;
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            at += 1;
            continue;
        }
        match b {
            b'"' if depth == 1 && text.get(at..)?.starts_with(&want) => {
                let start = at + want.len();
                return text.get(start..start + value_len(text.get(start..)?)?);
            }
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        at += 1;
    }
    None
}

/// The length of the one JSON value `text` starts with, in compact JSON.
fn value_len(text: &[u8]) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &b) in text.iter().enumerate() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => {
                    in_string = false;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ if depth == 0 && matches!(text.get(i + 1), Some(b',' | b'}') | None) => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// `json.Marshal` of a raw message's HTML escaping: `<`, `>` and `&`, and U+2028 and
/// U+2029, in strings, as `\u` escapes.
fn escape_html(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    let mut i = 0;
    while let Some(&b) = text.get(i) {
        if in_string && !escaped {
            let swap: Option<&[u8]> = match b {
                b'<' => Some(b"\\u003c"),
                b'>' => Some(b"\\u003e"),
                b'&' => Some(b"\\u0026"),
                0xe2 if text.get(i + 1) == Some(&0x80) && text.get(i + 2) == Some(&0xa8) => Some(b"\\u2028"),
                0xe2 if text.get(i + 1) == Some(&0x80) && text.get(i + 2) == Some(&0xa9) => Some(b"\\u2029"),
                _ => None,
            };
            if let Some(s) = swap {
                out.extend_from_slice(s);
                i += if b == 0xe2 { 3 } else { 1 };
                continue;
            }
        }
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else if b == b'"' {
            in_string = true;
        }
        out.push(b);
        i += 1;
    }
    out
}

/// `makeInTotoStatement`: the scanned statement with its own subjects, else the image's
/// (`subjects`: each name and the manifest's digest), written as Go writes
/// `intoto.Statement`.
pub fn intoto(s: &Scanned, subjects: &[(String, String)]) -> String {
    use super::provenance::Json;
    let listed: Vec<Json> = if s.subjects.is_empty() {
        subjects
            .iter()
            .map(|(name, digest)| {
                let (alg, hex) = digest.split_once(':').unwrap_or(("sha256", digest.as_str()));
                Json::Obj(vec![
                    ("name".into(), Json::Str(name.clone())),
                    (
                        "digest".into(),
                        Json::Obj(vec![(alg.to_string(), Json::Str(hex.to_string()))]),
                    ),
                ])
            })
            .collect()
    } else {
        s.subjects
            .iter()
            .map(|(name, digest)| {
                let name = if name.is_empty() {
                    "_".to_string()
                } else {
                    name.clone()
                };
                Json::Obj(vec![
                    ("name".into(), Json::Str(name)),
                    (
                        "digest".into(),
                        Json::Obj(
                            digest
                                .iter()
                                .map(|(k, v)| (k.clone(), Json::Str(v.clone())))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect()
    };
    let head = Json::Obj(vec![
        (
            "_type".into(),
            Json::Str("https://in-toto.io/Statement/v0.1".into()),
        ),
        ("predicateType".into(), Json::Str(s.predicate_type.clone())),
        ("subject".into(), Json::Arr(listed)),
    ])
    .compact();
    // The predicate as it is, after the header's fields.
    let mut out = head;
    out.pop();
    out.push_str(",\"predicate\":");
    out.push_str(&String::from_utf8_lossy(&s.predicate));
    out.push('}');
    out
}
