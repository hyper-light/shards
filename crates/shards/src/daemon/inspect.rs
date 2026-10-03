//! `shards image inspect` as dockerd's containerd store answers `docker image inspect`
//! (moby docker-v29.3.1 daemon/containerd/image_inspect.go; api/types/image
//! InspectResponse) and docker/cli prints it (cli/command/inspect/inspector.go,
//! IndentedInspector): each image's document in the API's field order, compact as Go
//! encodes it, then all of them as one array indented by encoding/json's Indent.

use std::time::SystemTime;

use shards_dockerfile::image::{Image as Config, json_string};
use shards_image::reference::Reference;
use shards_image::store::Image;

/// What moby's collectRepoTagsAndDigests makes of an image's references: each name,
/// familiar (a name with a digest among them); and each name's repository with the
/// image's ID. Dangling names give neither.
fn tags_and_digests(image: &Image) -> (Vec<String>, Vec<String>) {
    let (mut tags, mut digests): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let push = |list: &mut Vec<String>, s: String| {
        if !list.contains(&s) {
            list.push(s);
        }
    };
    for name in &image.references {
        if name.starts_with(super::rmi::DANGLING) {
            continue;
        }
        let Ok(r) = Reference::parse_normalized(name) else {
            push(&mut tags, name.clone());
            continue;
        };
        push(&mut tags, r.familiar());
        // A name with a digest names this image's ID: its repository with the ID is it.
        let mut digested = r;
        digested.tag = None;
        digested.digest = Some(image.id.clone());
        push(&mut digests, digested.familiar());
    }
    (tags, digests)
}

/// A JSON array of strings, as Go writes one.
fn strings(list: &[String]) -> String {
    let items: Vec<String> = list.iter().map(|s| json_string(s.as_bytes())).collect();
    format!("[{}]", items.join(","))
}

/// A time as Go's time.Time marshals it, in UTC: RFC 3339 with what fraction it has.
fn time_json(t: SystemTime) -> String {
    let since = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let mut time = shards_dockerfile::go::Time::from_unix(i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
    time.nanosecond = since.subsec_nanos();
    time.rfc3339_nano()
        .map_or_else(|_| "\"1970-01-01T00:00:00Z\"".into(), |s| format!("\"{s}\""))
}

/// `image`'s InspectResponse, compact: fields in the struct's order, those Go leaves out
/// when empty left out.
pub(super) fn document(image: &Image, record: Option<&str>) -> String {
    let (tags, digests) = tags_and_digests(image);
    let config = image.config.as_deref().and_then(|b| Config::from_json(b).ok());
    let mut o = vec![
        format!("\"Id\":{}", json_string(image.id.to_string().as_bytes())),
        format!("\"RepoTags\":{}", strings(&tags)),
        format!("\"RepoDigests\":{}", strings(&digests)),
    ];
    if let Some(comment) = config
        .as_ref()
        .and_then(|c| c.history.last())
        .map(|h| &h.comment)
        .filter(|c| !c.is_empty())
    {
        o.push(format!("\"Comment\":{}", json_string(comment)));
    }
    if let Some(created) = config
        .as_ref()
        .and_then(|c| c.created.as_ref())
        .and_then(|t| t.rfc3339_nano().ok())
    {
        o.push(format!("\"Created\":{}", json_string(created.as_bytes())));
    }
    if let Some(author) = config.as_ref().map(|c| &c.author).filter(|a| !a.is_empty()) {
        o.push(format!("\"Author\":{}", json_string(author)));
    }
    let empty = Vec::new();
    let platform = config.as_ref().map(|c| &c.platform);
    o.push(format!(
        "\"Config\":{}",
        config
            .as_ref()
            .map_or_else(|| "{}".to_string(), |c| c.config.to_json())
    ));
    o.push(format!(
        "\"Architecture\":{}",
        json_string(platform.map_or(&empty, |p| &p.architecture))
    ));
    if let Some(v) = platform.map(|p| &p.variant).filter(|v| !v.is_empty()) {
        o.push(format!("\"Variant\":{}", json_string(v)));
    }
    o.push(format!(
        "\"Os\":{}",
        json_string(platform.map_or(&empty, |p| &p.os))
    ));
    if let Some(v) = platform.map(|p| &p.os_version).filter(|v| !v.is_empty()) {
        o.push(format!("\"OsVersion\":{}", json_string(v)));
    }
    // The content here of our platform's image, walked from what its reference resolved
    // to: the index, if it is one, and our manifest, its config and layers. Its root
    // filesystem is not counted: dockerd 29.3.1 (containerd snapshotter) shows no
    // snapshot here, measured on four images (2026-10-02), where `images` counts it.
    let index = if image.target.digest == image.manifest.to_string() {
        0
    } else {
        u64::try_from(image.target.size).unwrap_or(0)
    };
    let size = image
        .manifests
        .iter()
        .find(|m| m.digest == image.manifest)
        .map_or(0, |m| m.content)
        .saturating_add(index);
    o.push(format!("\"Size\":{size}"));
    let mut rootfs = Vec::new();
    if let Some(c) = &config {
        if !c.rootfs.kind.is_empty() {
            rootfs.push(format!("\"Type\":{}", json_string(&c.rootfs.kind)));
        }
        if let Some(layers) = c.rootfs.diff_ids.as_ref().filter(|l| !l.is_empty()) {
            let items: Vec<String> = layers.iter().map(|l| json_string(l)).collect();
            rootfs.push(format!("\"Layers\":[{}]", items.join(",")));
        }
    }
    o.push(format!("\"RootFS\":{{{}}}", rootfs.join(",")));
    o.push(format!(
        "\"Metadata\":{{\"LastTagTime\":{}}}",
        time_json(image.tagged_at.unwrap_or(SystemTime::UNIX_EPOCH))
    ));
    // The record's own description of what it resolved to, if it keeps one.
    let t = record.and_then(|r| image.targets.get(r)).unwrap_or(&image.target);
    o.push(format!("\"Descriptor\":{}", descriptor(t)));
    let pulls: Vec<String> = image
        .sources
        .iter()
        .map(|s| format!("{{\"Repository\":{}}}", json_string(s.as_bytes())))
        .collect();
    // dockerd leaves out an identity it knows nothing of.
    if !pulls.is_empty() {
        o.push(format!("\"Identity\":{{\"Pull\":[{}]}}", pulls.join(",")));
    }
    format!("{{{}}}", o.join(","))
}

/// An ocispec.Descriptor as Go encodes it: mediaType, digest, size, then the annotations,
/// keys sorted, and the platform, each only if it has any.
fn descriptor(t: &shards_image::oci::Descriptor) -> String {
    let s = |v: &str| json_string(v.as_bytes());
    let mut d = format!(
        "{{\"mediaType\":{},\"digest\":{},\"size\":{}",
        s(&t.media_type),
        s(&t.digest),
        t.size
    );
    if !t.annotations.is_empty() {
        let pairs: Vec<String> = t
            .annotations
            .iter()
            .map(|(k, v)| format!("{}:{}", s(k), s(v)))
            .collect();
        d.push_str(&format!(",\"annotations\":{{{}}}", pairs.join(",")));
    }
    if let Some(p) = &t.platform {
        let mut fields = vec![
            format!("\"architecture\":{}", s(&p.architecture)),
            format!("\"os\":{}", s(&p.os)),
        ];
        if !p.os_features.is_empty() {
            let f: Vec<String> = p.os_features.iter().map(|x| s(x)).collect();
            fields.push(format!("\"os.features\":[{}]", f.join(",")));
        }
        if let Some(v) = p.variant.as_deref().filter(|v| !v.is_empty()) {
            fields.push(format!("\"variant\":{}", s(v)));
        }
        d.push_str(&format!(",\"platform\":{{{}}}", fields.join(",")));
    }
    d.push('}');
    d
}

/// encoding/json's Indent with no prefix and `indent`, of compact JSON: each member and
/// element on its own line, `: ` after keys, empty objects and arrays kept whole.
pub(super) fn indent(compact: &str, indent: &str) -> String {
    let mut out = String::with_capacity(compact.len() * 2);
    let (mut depth, mut need, mut string, mut escaped) = (0usize, false, false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent);
        }
    };
    for c in compact.chars() {
        if string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                string = false;
            }
            continue;
        }
        if need && c != '}' && c != ']' {
            need = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            '"' => {
                string = true;
                out.push(c);
            }
            '{' | '[' => {
                need = true;
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '}' | ']' => {
                if need {
                    need = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// The record a name given names: the name, with `latest` if it names no tag; none for
/// an ID or a digest.
fn record_name(given: &str) -> Option<String> {
    match shards_image::reference::AnyReference::parse(given) {
        Ok(shards_image::reference::AnyReference::Named(mut r)) if r.digest.is_none() => {
            if r.tag.is_none() {
                r.tag = Some("latest".into());
            }
            Some(r.to_string())
        }
        _ => None,
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards image inspect IMAGE...` (docker/cli inspect.Inspect): the documents of the
    /// images found, as one array, then what could not be found.
    pub(super) fn image_inspect(&self, args: &[String], reply: &super::commands::Reply<'_>) -> u8 {
        let images = match self.store() {
            Ok(Some(store)) => match store.images() {
                Ok(images) => images,
                Err(e) => {
                    reply.err(&format!("Error response from daemon: {e}"));
                    return 1;
                }
            },
            Ok(None) => Vec::new(),
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let (mut documents, mut errors) = (Vec::new(), Vec::new());
        for given in args {
            match super::images::resolve(&images, given) {
                Ok(image) => documents.push(document(image, record_name(given).as_deref())),
                Err(e) => errors.push(format!("Error response from daemon: {e}")),
            }
        }
        let text = if documents.is_empty() {
            "[]".to_string()
        } else {
            indent(&format!("[{}]", documents.join(",")), "    ")
        };
        reply.out(&text);
        if errors.is_empty() {
            return 0;
        }
        reply.err(&errors.join("\n"));
        1
    }
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards save IMAGE...` (moby ImageExport): every image found first, as dockerd
    /// finds them all before it streams; then the archive, written to what the client
    /// sent (its stdout, or its `-o` file), under a lease, so that no collection takes a
    /// blob meanwhile.
    pub(super) fn save(
        &self,
        args: &[String],
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        use shards_image::reference::AnyReference;
        let refuse = |said: &str| {
            reply.err(said);
            1
        };
        let Some(out) = asker.files.first() else {
            return refuse("shards: save: the client sent nowhere to write");
        };
        let store = match self.store() {
            Ok(Some(store)) => store,
            Ok(None) => {
                let given = args.first().map(String::as_str).unwrap_or_default();
                return refuse(&format!(
                    "Error response from daemon: {}",
                    super::images::not_found(given)
                ));
            }
            Err(e) => return refuse(&format!("Error response from daemon: {e}")),
        };
        let lease = store.lease();
        let images = match lease
            .as_ref()
            .map_err(ToString::to_string)
            .and_then(|_| store.images().map_err(|e| e.to_string()))
        {
            Ok(images) => images,
            Err(e) => return refuse(&format!("Error response from daemon: {e}")),
        };
        let mut asked = Vec::with_capacity(args.len());
        for given in args {
            let resolved = super::images::resolve(&images, given);
            let parsed = AnyReference::parse(given);
            // dockerd's ExportImage: a name that is no start of the ID it found, nor holds
            // a digest, and names a repository alone, is every tag of the repository,
            // each saved by its name, in name order.
            let by_id = resolved.as_ref().is_ok_and(|i| {
                let algorithm = format!("{}:", i.id.algorithm().name());
                i.id.hex()
                    .starts_with(given.strip_prefix(&algorithm).unwrap_or(given))
            });
            let digested = matches!(&parsed, Ok(AnyReference::Named(r)) if r.digest.is_some())
                || matches!(&parsed, Ok(AnyReference::Digest(_)));
            if !by_id
                && !digested
                && let Ok(AnyReference::Named(repo)) = &parsed
                && repo.tag.is_none()
            {
                let mut tagged: Vec<(String, &shards_image::store::Image)> = images
                    .iter()
                    .flat_map(|i| i.references.iter().map(move |r| (r, i)))
                    .filter(|(r, _)| {
                        shards_image::reference::Reference::parse_normalized(r)
                            .is_ok_and(|r| r.name() == repo.name() && r.tag.is_some() && r.digest.is_none())
                    })
                    .map(|(r, i)| (r.clone(), i))
                    .collect();
                tagged.sort_by(|a, b| a.0.cmp(&b.0));
                if tagged.is_empty() {
                    return refuse(&format!(
                        "Error response from daemon: No such image: {}:latest",
                        repo.familiar()
                    ));
                }
                for (name, image) in tagged {
                    asked.push(shards_image::save::Asked {
                        image,
                        name: Some(name),
                    });
                }
                continue;
            }
            let image = match resolved {
                Ok(image) => image,
                Err(e) => return refuse(&format!("Error response from daemon: {e}")),
            };
            // Asked by name, the name it was found by; by ID or digest, none.
            let name = match AnyReference::parse(given) {
                Ok(AnyReference::Named(mut r)) if r.digest.is_none() => {
                    if r.tag.is_none() {
                        r.tag = Some("latest".into());
                    }
                    Some(r.to_string()).filter(|n| image.references.contains(n))
                }
                _ => None,
            };
            asked.push(shards_image::save::Asked { image, name });
        }
        let written = out.try_clone().map_err(|e| e.to_string()).and_then(|fd| {
            let w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::from(fd));
            shards_image::save::save(&store, &asked, w).map_err(|e| e.to_string())
        });
        match written {
            Ok(()) => 0,
            Err(e) => refuse(&format!("Error response from daemon: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As encoding/json's Indent lays out what it is given, by Go's own answers
    /// (docker-images.json, `indents`), strings' punctuation untouched.
    #[test]
    fn json_indents_as_go_indents_it() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("docker-images.json")).unwrap();
        for case in golden["indents"].as_array().unwrap() {
            assert_eq!(
                indent(case["in"].as_str().unwrap(), "    "),
                case["out"].as_str().unwrap(),
                "{case}"
            );
        }
        assert_eq!(indent("[]", "    "), "[]");
        assert_eq!(
            indent(r#"{"a":[],"b":{}}"#, "  "),
            "{\n  \"a\": [],\n  \"b\": {}\n}"
        );
        assert_eq!(
            indent(r#"[{"Id":"x","L":[1,2],"S":"a,b:{c}[\"d\"]"}]"#, "    "),
            "[\n    {\n        \"Id\": \"x\",\n        \"L\": [\n            1,\n            2\n        ],\n        \"S\": \"a,b:{c}[\\\"d\\\"]\"\n    }\n]"
        );
    }
}
