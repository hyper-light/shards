//! What the Dockerfile frontend asks BuildKit to solve and returns to it, held to
//! docker/dockerfile:1 (dockerfile/1.27.1) on BuildKit v0.28.1 (Docker 29.3.1) as D113's
//! spy captured it in shards-dind (scripts/gateway/capture): the Dockerfile's own
//! definition, the build's, with its metadata, capabilities and source map, and the image
//! config and base config the build returns. BuildKit's client writes maps in Go's map
//! order, which nothing reads, so definitions are compared with each map's entries in one
//! order.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use shards_dockerfile::llb::{Definition, Input, Meta, Op, OpKind};
use shards_dockerfile::pb::{self, Carried, SourceInfo};
use shards_dockerfile::plan::{self, EpochSource, Options, Resolved, Resolver};
use shards_dockerfile::platform::Platform;

fn testdata(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/gateway")
            .join(name),
    )
    .unwrap()
}

/// The session BuildKit gave the captured build.
const SESSION: &[u8] = b"i7nf753w4qqfpv48b2v4war4t";

fn varint(b: &[u8], at: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let c = b[*at];
        *at += 1;
        v |= u64::from(c & 0x7f) << shift;
        shift += 7;
        if c < 0x80 {
            return v;
        }
    }
}

/// A message's fields: each tag and its value's bytes.
fn fields(b: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let tag = varint(b, &mut at);
        let value = match tag & 7 {
            0 => {
                let start = at;
                varint(b, &mut at);
                b[start..at].to_vec()
            }
            2 => {
                let n = varint(b, &mut at) as usize;
                at += n;
                b[at - n..at].to_vec()
            }
            w => panic!("wire type {w}"),
        };
        out.push((tag, value));
    }
    out
}

#[derive(Clone, Copy)]
enum Kind {
    Definition,
    OpMetadata,
    Source,
    SourceInfo,
}

fn flat(fields: Vec<(u64, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, v) in fields {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&(v.len() as u64).to_be_bytes());
        out.extend_from_slice(&v);
    }
    out
}

/// A message's fields with each map's entries in one order, the messages it holds read
/// the same way.
fn canonical(b: &[u8], kind: Kind) -> Vec<(u64, Vec<u8>)> {
    let mut plain = Vec::new();
    let mut maps = Vec::new();
    for (tag, v) in fields(b) {
        let field = tag >> 3;
        match (kind, field) {
            (Kind::Definition, 2) => {
                let entry = fields(&v)
                    .into_iter()
                    .map(|(t, x)| match t >> 3 {
                        2 => (t, flat(canonical(&x, Kind::OpMetadata))),
                        _ => (t, x),
                    })
                    .collect();
                maps.push((tag, flat(entry)));
            }
            (Kind::Definition, 3) => plain.push((tag, flat(canonical(&v, Kind::Source)))),
            (Kind::OpMetadata, 2 | 5) | (Kind::Source, 1) => maps.push((tag, v)),
            (Kind::Source, 2) => plain.push((tag, flat(canonical(&v, Kind::SourceInfo)))),
            (Kind::SourceInfo, 3) => plain.push((tag, flat(canonical(&v, Kind::Definition)))),
            _ => plain.push((tag, v)),
        }
    }
    maps.sort();
    plain.extend(maps);
    plain
}

/// dockerui's ReadEntrypoint source: the Dockerfile, its .dockerignore and Docker's other
/// casing, from the client's `dockerfile` directory.
fn dockerfile_definition() -> Definition {
    let mut attrs = BTreeMap::new();
    attrs.insert(b"local.differ".to_vec(), b"none".to_vec());
    attrs.insert(
        b"local.followpaths".to_vec(),
        br#"["Dockerfile","Dockerfile.dockerignore","dockerfile"]"#.to_vec(),
    );
    attrs.insert(b"local.session".to_vec(), SESSION.to_vec());
    attrs.insert(b"local.sharedkeyhint".to_vec(), b"dockerfile".to_vec());
    let mut meta = Meta::default();
    meta.description.insert(
        b"llb.customname".to_vec(),
        b"[internal] load build definition from Dockerfile".to_vec(),
    );
    Definition {
        ops: vec![Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: b"local://dockerfile".to_vec(),
                attrs,
            },
            platform: None,
        }],
        metadata: vec![meta],
        root: Some(Input { op: 0, index: 0 }),
    }
}

#[test]
fn the_dockerfiles_own_definition_is_docker_dockerfiles() {
    let m = pb::definition(&dockerfile_definition(), &Carried::default()).unwrap();
    let want = testdata("dockerfile-definition.pb");
    assert_eq!(
        canonical(&m.bytes, Kind::Definition),
        canonical(&want, Kind::Definition)
    );
    assert_eq!(
        String::from_utf8(m.digests[0].clone()).unwrap(),
        "sha256:b635cb1f9438712c91cad714732d6c7d8d5a391dfcd573e57a1f87331299f01a"
    );
    assert_eq!(
        String::from_utf8(m.root).unwrap(),
        "sha256:d9547ed027791375b98f4f46992ad15bfe735128bd704cdaf52391aa74aa8cf1"
    );
}

/// BuildKit's answers in the capture: alpine:3.20's config for linux/arm64, and no
/// .dockerignore in the context.
struct Captured;

impl Resolver for Captured {
    fn resolve(&self, name: &[u8], platform: &Platform, log: &[u8]) -> Result<Resolved, Vec<u8>> {
        // What docker/dockerfile asked: anything else is a different request.
        if name != b"docker.io/library/alpine:3.20"
            || platform != &Platform::new("linux", "arm64")
            || log != b"[internal] load metadata for docker.io/library/alpine:3.20"
        {
            return Err(b"not the captured request".to_vec());
        }
        Ok(Resolved {
            reference: name.to_vec(),
            digest: Some(b"sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc".to_vec()),
            config: testdata("alpine-3.20-arm64.json"),
        })
    }

    fn epoch(&self, _: &EpochSource) -> Result<Option<(i64, u32)>, Vec<u8>> {
        Ok(None)
    }

    fn context_excludes(&self) -> Result<Option<Vec<Vec<u8>>>, Vec<u8>> {
        Ok(Some(Vec::new()))
    }
}

/// The options the capture's build gave the frontend: no platform (the worker's first,
/// linux/arm64), `no-cache=`, `image-resolve-mode=local`, the `# syntax=` line's cmdline,
/// and BuildKit's session.
fn captured_options() -> Options {
    Options {
        target_platform: Platform::new("linux", "arm64"),
        implicit_target: true,
        build_platforms: vec![Platform::new("linux", "arm64")],
        no_cache: Some(Vec::new()),
        image_resolve_mode: b"local".to_vec(),
        session: SESSION.to_vec(),
        cmdline: Some(b"127.0.0.1:15113/shards-d113-spy:1".to_vec()),
        ..Options::default()
    }
}

#[test]
fn a_dockerfiles_build_is_planned_as_docker_dockerfile_plans_it() {
    let text = testdata("build.Dockerfile");
    let planned = plan::plan(&text, &captured_options(), &Captured).unwrap();
    let dockerfile = pb::definition(&dockerfile_definition(), &Carried::default()).unwrap();
    let carried = Carried {
        source: Some(SourceInfo {
            filename: b"Dockerfile",
            language: b"Dockerfile",
            data: &text,
            definition: Some(&dockerfile.bytes),
        }),
        sets_default_path: true,
        group_prefix: "",
    };
    let m = pb::definition(&planned.definition(), &carried).unwrap();
    let want = testdata("build-definition.pb");
    assert_eq!(
        canonical(&m.bytes, Kind::Definition),
        canonical(&want, Kind::Definition)
    );
    // The config and the base's, as the frontend returns them (dockerui's Build).
    assert_eq!(
        planned.image.to_json().unwrap().into_bytes(),
        testdata("build-config.json")
    );
    assert_eq!(
        planned.base_image.unwrap().to_json().unwrap().into_bytes(),
        testdata("build-base-config.json")
    );
}
