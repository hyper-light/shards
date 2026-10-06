//! The build cache (docs/design/architecture.md D50): a step whose definition and inputs a
//! build has met before is not run again; its result is the layers it made then, as
//! BuildKit's solver reuses a vertex its cache key finds (`#N CACHED`).
//!
//! A step's key is the SHA-256 of shards' version, its definition and each input's key, in
//! its inputs' order (not where they sit in the plan, which another build lays out
//! otherwise). An input that a chain of layers makes (a base image, an earlier step) is
//! keyed by what made it; one that none makes (the build context, a download, a Git
//! checkout) by its content: each path's kind, mode, owner, extended attributes, link
//! target and bytes, as BuildKit's content checksums take them, without times. A record
//! keeps each of the step's outputs as its layers, held in the store from collection while
//! it is there.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};
use shards_build::data::Sources;
use shards_build::vfs::Fs;
use shards_dockerfile::export::Layer;
use shards_dockerfile::llb::Op;
use shards_image::erofs::{Kind, NodeId, Source as _, Tree};

/// The version of what a key covers: a change to how steps are made changes it.
const FORMAT: &[u8] = b"shards build cache 1";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn framed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

/// Step `op`'s key, its inputs keyed by `inputs`, in its inputs' order.
pub fn op_key(op: &Op, inputs: &[&str]) -> String {
    let mut h = Sha256::new();
    framed(&mut h, FORMAT);
    framed(&mut h, env!("CARGO_PKG_VERSION").as_bytes());
    framed(&mut h, format!("{:?}", op.kind).as_bytes());
    framed(&mut h, format!("{:?}", op.platform).as_bytes());
    for (input, key) in op.inputs.iter().zip(inputs) {
        h.update(input.index.to_be_bytes());
        framed(&mut h, key.as_bytes());
    }
    hex(&h.finalize())
}

/// A source's key, from what it is (`identifier`) and what it holds (`content`).
pub fn source_key(identifier: &[u8], content: &str) -> String {
    let mut h = Sha256::new();
    framed(&mut h, FORMAT);
    framed(&mut h, identifier);
    framed(&mut h, content.as_bytes());
    hex(&h.finalize())
}

/// The key of a snapshot's content: every path under its root, in name order.
pub fn content_key(fs: &Fs, sources: &mut Sources) -> Result<String, String> {
    let tree = fs.tree();
    let mut h = Sha256::new();
    framed(&mut h, FORMAT);
    let mut stack: Vec<(Vec<u8>, NodeId)> = vec![(Vec::new(), Tree::ROOT)];
    let mut buf = vec![0u8; 1 << 16];
    while let Some((path, id)) = stack.pop() {
        let node = tree.node(id).ok_or("a snapshot names a node it lacks")?;
        framed(&mut h, &path);
        h.update(node.meta.mode.to_be_bytes());
        h.update(node.meta.uid.to_be_bytes());
        h.update(node.meta.gid.to_be_bytes());
        for (k, v) in node.meta.xattrs.iter() {
            framed(&mut h, k);
            framed(&mut h, v);
        }
        match &node.kind {
            Kind::Dir(_) => {
                h.update(b"d");
                // Pushed in reverse, to be walked in name order.
                let mut entries = tree.entries(id);
                entries.reverse();
                for (name, child) in entries {
                    let mut p = path.clone();
                    p.push(b'/');
                    p.extend_from_slice(name);
                    stack.push((p, child));
                }
            }
            Kind::File { size, data } => {
                h.update(b"f");
                h.update(size.to_be_bytes());
                let mut at = 0u64;
                while at < *size {
                    let n = usize::try_from((*size - at).min(buf.len() as u64)).unwrap_or(buf.len());
                    let chunk = buf.get_mut(..n).ok_or("a chunk past the buffer")?;
                    sources
                        .read_at(*data, at, chunk)
                        .map_err(|e| format!("reading {}: {e}", String::from_utf8_lossy(&path)))?;
                    h.update(&*chunk);
                    at += n as u64;
                }
            }
            Kind::Symlink(target) => {
                h.update(b"l");
                framed(&mut h, target);
            }
            Kind::CharDevice { major, minor } => {
                h.update(b"c");
                h.update(major.to_be_bytes());
                h.update(minor.to_be_bytes());
            }
            Kind::BlockDevice { major, minor } => {
                h.update(b"b");
                h.update(major.to_be_bytes());
                h.update(minor.to_be_bytes());
            }
            Kind::Fifo => h.update(b"p"),
            #[allow(unreachable_patterns)]
            _ => h.update(b"s"),
        }
    }
    Ok(hex(&h.finalize()))
}

/// A record's body: each output's layers, as the build made them.
pub fn encode(outputs: &[Vec<Layer>]) -> Result<String, String> {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let outputs: Vec<serde_json::Value> = outputs
        .iter()
        .map(|layers| {
            layers
                .iter()
                .map(|l| {
                    let annotations: BTreeMap<String, String> =
                        l.annotations.iter().map(|(k, v)| (text(k), text(v))).collect();
                    let created = l
                        .created
                        .as_ref()
                        .map(|t| t.rfc3339_nano())
                        .transpose()
                        .map_err(|e| text(&e))?;
                    Ok(serde_json::json!({
                        "mediaType": text(&l.media_type),
                        "digest": text(&l.digest),
                        "size": l.size,
                        "diffID": text(&l.diff_id),
                        "annotations": annotations,
                        "created": created,
                        "description": text(&l.description),
                    }))
                })
                .collect::<Result<Vec<_>, String>>()
                .map(serde_json::Value::Array)
        })
        .collect::<Result<_, String>>()?;
    serde_json::to_string(&outputs).map_err(|e| e.to_string())
}

/// The layers of each output a record's body holds.
pub fn decode(body: &[u8]) -> Result<Vec<Vec<Layer>>, String> {
    let outputs: Vec<Vec<serde_json::Value>> = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let field =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.as_bytes().to_vec());
    outputs
        .into_iter()
        .map(|layers| {
            layers
                .into_iter()
                .map(|v| {
                    let annotations = v
                        .get("annotations")
                        .and_then(|a| a.as_object())
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, x)| {
                                    Some((k.as_bytes().to_vec(), x.as_str()?.as_bytes().to_vec()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let created = match field(&v, "created") {
                        Some(t) => Some(
                            shards_dockerfile::go::parse_rfc3339(&t)
                                .map_err(|e| String::from_utf8_lossy(&e).into_owned())?,
                        ),
                        None => None,
                    };
                    Ok(Layer {
                        media_type: field(&v, "mediaType").ok_or("a cached layer without a media type")?,
                        digest: field(&v, "digest").ok_or("a cached layer without a digest")?,
                        size: v
                            .get("size")
                            .and_then(serde_json::Value::as_u64)
                            .ok_or("a cached layer without a size")?,
                        diff_id: field(&v, "diffID").ok_or("a cached layer without a diff ID")?,
                        annotations,
                        created,
                        description: field(&v, "description").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shards_dockerfile::llb::{Input, OpKind};

    fn source(id: &str) -> Op {
        Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: id.as_bytes().to_vec(),
                attrs: BTreeMap::new(),
            },
            platform: None,
        }
    }

    #[test]
    fn steps_are_keyed_by_what_they_are_and_read() {
        let a = source("docker-image://a");
        assert_eq!(op_key(&a, &[]), op_key(&a, &[]));
        assert_ne!(op_key(&a, &[]), op_key(&source("docker-image://b"), &[]));
        // Where an input sits in the plan does not count; what it is does.
        let merge = |at: usize| Op {
            inputs: vec![Input { op: at, index: 0 }],
            kind: OpKind::Merge,
            platform: None,
        };
        assert_eq!(op_key(&merge(1), &["k"]), op_key(&merge(7), &["k"]));
        assert_ne!(op_key(&merge(1), &["k"]), op_key(&merge(1), &["j"]));
        assert_ne!(
            source_key(b"local://context", "x"),
            source_key(b"local://context", "y")
        );
    }

    #[test]
    fn records_keep_their_layers_whole() {
        let layer = Layer {
            media_type: b"application/vnd.oci.image.layer.v1.tar".to_vec(),
            digest: b"sha256:aa".to_vec(),
            size: 7,
            diff_id: b"sha256:bb".to_vec(),
            annotations: [(b"k".to_vec(), b"v".to_vec())].into_iter().collect(),
            created: Some(shards_dockerfile::go::Time::from_unix(1_600_000_000)),
            description: b"RUN x".to_vec(),
        };
        let outputs = vec![vec![layer.clone()], Vec::new()];
        assert_eq!(decode(encode(&outputs).unwrap().as_bytes()).unwrap(), outputs);
    }
}
