//! The frontend's inputs (D130): definitions BuildKit gives the frontend whole, as buildx
//! gives one bake target's result to another (`target:` contexts) and a remote bake
//! definition's context, read as `grpcclient.Inputs` reads each (`llb.NewDefinitionOp`).

use std::collections::BTreeMap;

use shards_agentfile::llb::{
    Embedded, EmbeddedOp, GivenSource, Graph, GroupId, LinuxResources, Meta, Position, ProgressGroup, Range,
    RawSource, State,
};
use shards_agentfile::pb::{self, Carried};
use shards_agentfile::platform::Platform;
use shards_gateway::wire::{Error, Reader, Value};

/// Data no protobuf encoder writes, in protobuf-go's words.
fn invalid(_: Error) -> String {
    "proto: cannot parse invalid wire-format data".to_string()
}

/// A map entry's key and value, empty where left out.
fn entry(b: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (mut k, mut v) = (Vec::new(), Vec::new());
    for f in Reader::new(b) {
        match f.map_err(invalid)? {
            (1, Value::Bytes(x)) => k = x.to_vec(),
            (2, Value::Bytes(x)) => v = x.to_vec(),
            _ => {}
        }
    }
    Ok((k, v))
}

/// A definition read as protobuf-go reads `pb.Definition`, its parts not yet put
/// together: its ops, each one's metadata and locations by digest, and its source maps,
/// each with its definition's bytes where it has one.
#[derive(Default)]
struct Shallow {
    ops: Vec<EmbeddedOp>,
    metadata: BTreeMap<Vec<u8>, Meta>,
    locations: BTreeMap<Vec<u8>, Vec<(i32, Vec<Range>)>>,
    infos: Vec<GivenSource>,
    info_definitions: Vec<Option<Vec<u8>>>,
}

/// `def` read, and refused where `llb.NewDefinitionOp` refuses it before it reads its
/// source maps: a definition of no metadata (`Definition.IsNil`), an op that is no
/// `pb.Op`.
fn shallow(def: &[u8]) -> Result<Shallow, String> {
    let mut s = Shallow::default();
    let mut ops: Vec<Vec<u8>> = Vec::new();
    let mut has_metadata = false;
    for f in Reader::new(def) {
        match f.map_err(invalid)? {
            (1, Value::Bytes(op)) => ops.push(op.to_vec()),
            (2, Value::Bytes(e)) => {
                has_metadata = true;
                let (k, v) = entry(e)?;
                s.metadata.insert(k, op_metadata(&v)?);
            }
            (3, Value::Bytes(source)) => read_source(source, &mut s)?,
            _ => {}
        }
    }
    if !has_metadata {
        return Err("invalid nil input definition to definition op".into());
    }
    for bytes in ops {
        s.ops
            .push(op(bytes).map_err(|e| format!("failed to parse llb proto op: {e}"))?);
    }
    Ok(s)
}

/// `pb.Op`: what it reads, and the source it is, where its oneof ends one.
fn op(bytes: Vec<u8>) -> Result<EmbeddedOp, String> {
    let mut op = EmbeddedOp::default();
    let mut source: Option<RawSource> = None;
    let mut last = 0;
    for f in Reader::new(&bytes) {
        match f.map_err(invalid)? {
            (1, Value::Bytes(input)) => {
                let (mut digest, mut index) = (Vec::new(), 0i64);
                for g in Reader::new(input) {
                    match g.map_err(invalid)? {
                        (1, Value::Bytes(d)) => digest = d.to_vec(),
                        (2, Value::Varint(v)) => index = v as i64,
                        _ => {}
                    }
                }
                op.inputs.push((digest, index));
            }
            // A oneof's message given again merges into the one it holds.
            (n @ 2..=7, Value::Bytes(m)) => {
                if n != last || n != 3 {
                    source = None;
                }
                last = n;
                if n == 3 {
                    let (identifier, attrs) = source.get_or_insert_default();
                    for g in Reader::new(m) {
                        match g.map_err(invalid)? {
                            (1, Value::Bytes(id)) if !id.is_empty() => *identifier = id.to_vec(),
                            (2, Value::Bytes(e)) => {
                                let (k, v) = entry(e)?;
                                attrs.insert(k, v);
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    op.source = source;
    op.bytes = bytes;
    Ok(op)
}

/// `pb.OpMetadata`, as `llb.NewOpMetadata` keeps it.
fn op_metadata(b: &[u8]) -> Result<Meta, String> {
    let mut m = Meta::default();
    for f in Reader::new(b) {
        match f.map_err(invalid)? {
            (1, Value::Varint(v)) => m.ignore_cache = v != 0,
            (2, Value::Bytes(e)) => {
                let (k, v) = entry(e)?;
                m.description.insert(k, v);
            }
            (4, Value::Bytes(e)) => {
                let mut value = m.export_cache.unwrap_or_default();
                for g in Reader::new(e) {
                    if let (1, Value::Varint(v)) = g.map_err(invalid)? {
                        value = v != 0;
                    }
                }
                m.export_cache = Some(value);
            }
            (5, Value::Bytes(e)) => {
                m.caps.insert(entry(e)?.0);
            }
            (6, Value::Bytes(e)) => {
                let g = m.progress_group.get_or_insert(ProgressGroup {
                    id: GroupId::Given(Vec::new()),
                    name: Vec::new(),
                    weak: false,
                });
                for f in Reader::new(e) {
                    match f.map_err(invalid)? {
                        (1, Value::Bytes(id)) if !id.is_empty() => g.id = GroupId::Given(id.to_vec()),
                        (2, Value::Bytes(name)) if !name.is_empty() => g.name = name.to_vec(),
                        (3, Value::Varint(v)) => g.weak = v != 0,
                        _ => {}
                    }
                }
            }
            (7, Value::Bytes(e)) => {
                let r = m.linux_resources.get_or_insert_with(LinuxResources::default);
                for f in Reader::new(e) {
                    match f.map_err(invalid)? {
                        (1, Value::Varint(v)) => r.memory = v as i64,
                        (2, Value::Varint(v)) => r.memory_swap = v as i64,
                        (3, Value::Varint(v)) => r.cpu_shares = v,
                        (4, Value::Varint(v)) => r.cpu_period = v,
                        (5, Value::Varint(v)) => r.cpu_quota = v as i64,
                        (6, Value::Bytes(c)) if !c.is_empty() => r.cpuset_cpus = c.to_vec(),
                        (7, Value::Bytes(c)) if !c.is_empty() => r.cpuset_mems = c.to_vec(),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(m)
}

/// `pb.Source`: each op's locations, by its digest, and the source maps.
fn read_source(b: &[u8], s: &mut Shallow) -> Result<(), String> {
    for f in Reader::new(b) {
        match f.map_err(invalid)? {
            (1, Value::Bytes(e)) => {
                let (digest, locations) = entry(e)?;
                let mut list = Vec::new();
                for lf in Reader::new(&locations) {
                    let (1, Value::Bytes(loc)) = lf.map_err(invalid)? else {
                        continue;
                    };
                    let (mut index, mut ranges) = (0i32, Vec::new());
                    for f in Reader::new(loc) {
                        match f.map_err(invalid)? {
                            // int32: the low 32 bits of the varint, as protobuf-go reads it.
                            (1, Value::Varint(v)) => index = v as i32,
                            (2, Value::Bytes(r)) => ranges.push(range(r)?),
                            _ => {}
                        }
                    }
                    list.push((index, ranges));
                }
                s.locations.insert(digest, list);
            }
            (2, Value::Bytes(info)) => {
                let (mut g, mut definition) = (GivenSource::default(), None);
                for f in Reader::new(info) {
                    match f.map_err(invalid)? {
                        (1, Value::Bytes(x)) => g.filename = x.to_vec(),
                        (2, Value::Bytes(x)) => g.data = x.to_vec(),
                        (3, Value::Bytes(x)) => definition = Some(x.to_vec()),
                        (4, Value::Bytes(x)) => g.language = x.to_vec(),
                        _ => {}
                    }
                }
                s.infos.push(g);
                s.info_definitions.push(definition);
            }
            _ => {}
        }
    }
    Ok(())
}

/// `pb.Range`: each end, where set.
fn range(b: &[u8]) -> Result<Range, String> {
    let position = |b: &[u8]| -> Result<Position, String> {
        let mut p = Position::default();
        for f in Reader::new(b) {
            match f.map_err(invalid)? {
                (1, Value::Varint(v)) => p.line = v as i32,
                (2, Value::Varint(v)) => p.character = v as i32,
                _ => {}
            }
        }
        Ok(p)
    };
    let mut r = Range::default();
    for f in Reader::new(b) {
        match f.map_err(invalid)? {
            (1, Value::Bytes(p)) => r.start = Some(position(p)?),
            (2, Value::Bytes(p)) => r.end = Some(position(p)?),
            _ => {}
        }
    }
    Ok(r)
}

/// What `llb.NewDefinitionOp` refuses once it has read a definition's source maps: a
/// location in a map it lacks, a last op that reads nothing.
fn check(s: &Shallow) -> Result<(), String> {
    for list in s.locations.values() {
        for (index, _) in list {
            if !usize::try_from(*index).is_ok_and(|i| i < s.infos.len()) {
                return Err(format!("failed to find source map with index {index}"));
            }
        }
    }
    if s.ops.last().is_some_and(|last| last.inputs.is_empty()) {
        return Err("invalid definition with no inputs on last vertex".into());
    }
    Ok(())
}

/// A definition read whole: each op with its metadata and locations, each source map
/// with its definition as `made` holds it.
fn embedded(s: &Shallow, maps: &[Option<usize>], made: &[Option<(Vec<u8>, String)>]) -> Embedded {
    let ops = s
        .ops
        .iter()
        .map(|op| {
            let digest = pb::digest(&op.bytes);
            EmbeddedOp {
                metadata: s.metadata.get(&digest).cloned(),
                locations: s
                    .locations
                    .get(&digest)
                    .map(|list| {
                        list.iter()
                            .filter_map(|(index, ranges)| {
                                Some((usize::try_from(*index).ok()?, ranges.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                ..op.clone()
            }
        })
        .collect();
    let sources = s
        .infos
        .iter()
        .zip(maps)
        .map(|(info, map)| GivenSource {
            definition: map.and_then(|m| made.get(m).cloned().flatten()),
            ..info.clone()
        })
        .collect();
    Embedded { ops, sources }
}

/// `def`, a frontend's input, read as `llb.NewDefinitionOp` reads it: refused where that
/// refuses it, a source map's definition (read the same way) before the definition's own
/// locations; and where BuildKit's marshal of it refuses it, refused as it is read (D130).
/// Each source map's definition is kept as `sourceMapCollector.Marshal` writes it again.
/// Definitions in maps of maps are read without recursion, each a node.
pub(super) fn read(def: &[u8], platform: &Platform) -> Result<Embedded, String> {
    struct Node {
        s: Shallow,
        maps: Vec<Option<usize>>,
    }
    let node = |s: Shallow| Node {
        maps: vec![None; s.infos.len()],
        s,
    };
    let mut nodes = vec![node(shallow(def)?)];
    // Depth first, each node and the next of its maps to read.
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    while let Some(top) = stack.last_mut() {
        let (n, next) = *top;
        let Some(current) = nodes.get(n) else {
            return Err("a frontend input's source map read from nowhere".into());
        };
        if next < current.s.info_definitions.len() {
            top.1 += 1;
            if let Some(bytes) = current.s.info_definitions.get(next).cloned().flatten() {
                let child = nodes.len();
                nodes.push(node(shallow(&bytes)?));
                if let Some(slot) = nodes.get_mut(n).and_then(|p| p.maps.get_mut(next)) {
                    *slot = Some(child);
                }
                stack.push((child, 0));
            }
            continue;
        }
        check(&current.s)?;
        stack.pop();
    }
    // A node's maps come after it: from the last, each map's definition is ready when
    // the node that holds it is put together.
    let mut made: Vec<Option<(Vec<u8>, String)>> = vec![None; nodes.len()];
    for (n, node) in nodes.iter().enumerate().skip(1).rev() {
        let def = canonical(&embedded(&node.s, &node.maps, &made), platform)?;
        if let Some(slot) = made.get_mut(n) {
            *slot = Some(def);
        }
    }
    let first = nodes.first().ok_or("a frontend input of no definition")?;
    let top = embedded(&first.s, &first.maps, &made);
    Graph::default().embed(&top)?;
    Ok(top)
}

/// `given` marshalled anew, as `State.Marshal` of `llb.NewDefinitionOp`'s state writes
/// it: as protobuf, and as encoding/json writes it.
pub(super) fn canonical(given: &Embedded, platform: &Platform) -> Result<(Vec<u8>, String), String> {
    let mut graph = Graph::default();
    let state = State {
        output: graph.embed(given)?,
        ..State::scratch()
    };
    let def = graph.marshal(&state, platform);
    let carried = Carried::default();
    let bytes = pb::definition(&def, &carried).ok_or("failed to marshal a frontend input")?;
    let json = pb::definition_json(&def, &carried).ok_or("failed to marshal a frontend input")?;
    Ok((bytes.bytes, json))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use shards_agentfile::llb::{Action, CopyInfo, Graph, Meta, State};
    use shards_agentfile::pb::{self, Carried, SourceInfo};
    use shards_agentfile::platform::Platform;
    use shards_gateway::wire::Writer;

    use super::*;

    fn named(name: &[u8]) -> Meta {
        let mut m = Meta::default();
        m.description.insert(b"llb.customname".to_vec(), name.to_vec());
        m
    }

    fn local(name: &[u8]) -> (Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>) {
        let mut attrs = BTreeMap::new();
        attrs.insert(b"local.session".to_vec(), b"s".to_vec());
        attrs.insert(b"local.sharedkeyhint".to_vec(), name.to_vec());
        ([b"local://".as_slice(), name].concat(), attrs)
    }

    /// A definition as a Dockerfile's build marshals it, and buildx gives one bake
    /// target's to another: a copy from the context, located in the Dockerfile, whose
    /// source map carries the definition that loads it. Read and marshalled anew, it is
    /// what it was, byte for byte, and so is the map's definition, in JSON too.
    #[test]
    fn a_definition_given_is_marshalled_anew_as_it_came() {
        let platform = Platform::new("linux", "arm64");
        let mut lg = Graph::default();
        let (id, attrs) = local(b"dockerfile");
        let st = lg.source(
            id,
            attrs,
            None,
            named(b"[internal] load build definition from Dockerfile"),
        );
        let loading_def = lg.marshal(&st, &platform);
        let loading = pb::definition(&loading_def, &Carried::default()).unwrap();
        let mut g = Graph::default();
        let (id, attrs) = local(b"context");
        let ctx = g.source(id, attrs, None, named(b"[internal] load build context"));
        let mut meta = named(b"[base 1/1] COPY a /a");
        meta.locations.push(vec![(2, 2)]);
        let copy = Action::Copy {
            from: ctx.output,
            from_dir: b"/".to_vec(),
            src: b"/a".to_vec(),
            dest: b"/a".to_vec(),
            info: CopyInfo::default(),
        };
        let made = g.file(&State::scratch(), vec![copy], meta);
        let def = g.marshal(&made, &platform);
        let carried = Carried {
            source: Some(SourceInfo {
                filename: b"Dockerfile",
                language: b"Dockerfile",
                data: b"FROM scratch AS base\nCOPY a /a\n",
                definition: Some(&loading.bytes),
            }),
            sets_default_path: true,
            group_prefix: "s-",
        };
        let given = pb::definition(&def, &carried).unwrap();
        let read = read(&given.bytes, &platform).unwrap();
        let (again, _) = canonical(&read, &platform).unwrap();
        assert_eq!(again, given.bytes);
        let map = read.sources[0].definition.as_ref().unwrap();
        assert_eq!(map.0, loading.bytes);
        assert_eq!(
            map.1,
            pb::definition_json(&loading_def, &Carried::default()).unwrap()
        );
    }

    fn definition(ops: &[Vec<u8>], metadata: &[&[u8]], source: Option<&[u8]>) -> Vec<u8> {
        let mut w = Writer::default();
        for op in ops {
            w.message(1, op);
        }
        for digest in metadata {
            let mut e = Writer::default();
            e.message(1, digest);
            e.message(2, &[]);
            w.message(2, &e.0);
        }
        if let Some(s) = source {
            w.message(3, s);
        }
        w.0
    }

    /// A source with `infos` maps (each with `definition`, where given) and a location
    /// of `op` in map `index`.
    fn source(op: &[u8], index: i32, infos: &[Option<&[u8]>]) -> Vec<u8> {
        let mut loc = Writer::default();
        loc.int32(1, index);
        let mut locs = Writer::default();
        locs.message(1, &loc.0);
        let mut entry = Writer::default();
        entry.message(1, op);
        entry.message(2, &locs.0);
        let mut s = Writer::default();
        s.message(1, &entry.0);
        for d in infos {
            let mut info = Writer::default();
            info.string(1, "Dockerfile");
            if let Some(d) = d {
                info.message(3, d);
            }
            s.message(2, &info.0);
        }
        s.0
    }

    /// What `llb.NewDefinitionOp` refuses, in its words and its order: a definition of
    /// no metadata; a location in a map it lacks, after the maps' own definitions are
    /// read; a last op that reads nothing. And, as it is read, what BuildKit's marshal of
    /// it refuses: an op of no metadata.
    #[test]
    fn a_definition_given_is_refused() {
        let platform = Platform::new("linux", "arm64");
        let src = pb::op(
            &shards_agentfile::llb::Op {
                inputs: Vec::new(),
                kind: shards_agentfile::llb::OpKind::Source {
                    identifier: b"local://context".to_vec(),
                    attrs: BTreeMap::new(),
                },
                platform: None,
            },
            &[],
        )
        .unwrap();
        let digest = pb::digest(&src);
        let root = pb::root(&digest, 0);
        let root_digest = pb::digest(&root);
        let nil = "invalid nil input definition to definition op";
        assert_eq!(read(&[], &platform), Err(nil.to_string()));
        let good = definition(&[src.clone(), root.clone()], &[&digest, &root_digest], None);
        assert!(read(&good, &platform).is_ok());
        let bad_index = definition(
            &[src.clone(), root.clone()],
            &[&digest, &root_digest],
            Some(&source(&digest, 1, &[None])),
        );
        assert_eq!(
            read(&bad_index, &platform),
            Err("failed to find source map with index 1".to_string())
        );
        // A map's definition of no metadata: refused before the location.
        let bad_map = definition(
            &[src.clone(), root.clone()],
            &[&digest, &root_digest],
            Some(&source(&digest, 1, &[Some(&[])])),
        );
        assert_eq!(read(&bad_map, &platform), Err(nil.to_string()));
        let no_input = definition(std::slice::from_ref(&src), &[&digest], None);
        assert_eq!(
            read(&no_input, &platform),
            Err("invalid definition with no inputs on last vertex".to_string())
        );
        let no_metas = definition(&[src, root], &[&root_digest], None);
        assert_eq!(
            read(&no_metas, &platform),
            Err(format!(
                "invalid definition op with unknown metas \"{}\"",
                String::from_utf8_lossy(&digest)
            ))
        );
    }
}
