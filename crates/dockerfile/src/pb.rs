//! LLB operations as BuildKit marshals them (D80): `pb.Op` (solver/pb/ops.proto) in
//! protobuf's wire format, as protobuf-go's deterministic marshal writes it
//! (`client/llb` marshals each op so, and names it by its bytes' digest): fields in their
//! numbers' order, proto3's zero values left out, set messages written however empty,
//! repeated fields each element, and map entries by their keys' order, key and value both.
//! Its order is protobuf-go's legacy one (`order.LegacyFieldOrder`): a message's oneof
//! after its other fields, so an op's inputs, platform and constraints come before what
//! it does.
//! What provenance's `mode=max` records of a build (its LLB definition) is these.

use sha2::Digest as _;

use crate::llb::{
    Definition, Device, LinuxResources, Meta, NetMode, Op, OpAction, OpActionKind, OpChown, OpKind, OpMount,
    OpMountKind, OpUser, Process, Security, Sharing,
};
use crate::platform::Platform;

const VARINT: u8 = 0;
/// The field shards' own SKILL step is written in: past every `pb.Op` field.
const SKILLS: u32 = 100;
const LEN: u8 = 2;

/// A message being written.
#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn tag(&mut self, field: u32, wire: u8) {
        self.varint(u64::from(field) << 3 | u64::from(wire));
    }

    /// A uint32, uint64 or enum field, left out at zero.
    fn uint(&mut self, field: u32, v: u64) {
        if v != 0 {
            self.tag(field, VARINT);
            self.varint(v);
        }
    }

    /// An int64 field: two's complement, ten bytes when negative.
    fn int64(&mut self, field: u32, v: i64) {
        self.uint(field, v as u64);
    }

    /// An int32 field: sign-extended to 64 bits, as protobuf writes it.
    fn int32(&mut self, field: u32, v: i32) {
        self.uint(field, i64::from(v) as u64);
    }

    fn bool(&mut self, field: u32, v: bool) {
        self.uint(field, u64::from(v));
    }

    /// A string or bytes field, left out when empty.
    fn bytes(&mut self, field: u32, v: &[u8]) {
        if !v.is_empty() {
            self.always(field, v);
        }
    }

    /// A length-delimited field written whatever its length: a set message, an element of
    /// a repeated field, a map entry's key or value.
    fn always(&mut self, field: u32, v: &[u8]) {
        self.tag(field, LEN);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
    }

    fn message(&mut self, field: u32, m: W) {
        self.always(field, &m.0);
    }
}

/// `op`'s bytes, its inputs named by `digests` (each input's op's digest, in order).
pub fn op(op: &Op, digests: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut w = W::default();
    for (input, digest) in op.inputs.iter().zip(digests) {
        let mut i = W::default();
        i.bytes(1, digest);
        i.int64(2, input.index);
        w.message(1, i);
    }
    if let Some(p) = &op.platform {
        w.message(10, platform(p));
    }
    // `llb.Constraints` always sets the op's worker constraints, empty for a Dockerfile.
    w.message(11, W::default());
    match &op.kind {
        OpKind::Exec {
            process,
            mounts,
            network,
            security,
            secret_env,
            devices,
        } => {
            let mut e = W::default();
            e.message(1, meta(process));
            for m in mounts {
                e.message(2, mount(m));
            }
            e.uint(
                3,
                match network {
                    NetMode::Sandbox => 0,
                    NetMode::Host => 1,
                    NetMode::None => 2,
                },
            );
            e.uint(
                4,
                match security {
                    Security::Sandbox => 0,
                    Security::Insecure => 1,
                },
            );
            for (id, name, optional) in secret_env {
                let mut s = W::default();
                s.bytes(1, id);
                s.bytes(2, name);
                s.bool(3, *optional);
                e.message(5, s);
            }
            for d in devices {
                e.message(6, device(d));
            }
            w.message(2, e);
        }
        OpKind::Source { identifier, attrs } => {
            let mut s = W::default();
            s.bytes(1, identifier);
            for (k, v) in attrs {
                let mut entry = W::default();
                entry.always(1, k);
                entry.always(2, v);
                s.message(2, entry);
            }
            w.message(3, s);
        }
        OpKind::File { actions } => {
            let mut f = W::default();
            for a in actions {
                f.message(2, action(a));
            }
            w.message(4, f);
        }
        OpKind::Merge => {
            let mut m = W::default();
            for i in 0..op.inputs.len() {
                let mut input = W::default();
                input.int64(1, i64::try_from(i).ok()?);
                m.message(1, input);
            }
            w.message(6, m);
        }
        // shards' own step (D54), which BuildKit has no message for: in a field of no
        // `pb.Op`'s, which a BuildKit reader skips, so that its digest is its own.
        OpKind::Skills { name } => {
            let mut k = W::default();
            k.bytes(1, name);
            w.message(SKILLS, k);
        }
    }
    Some(w.0)
}

/// The definition's last op, which names its result: output `index` of the op `digest`
/// names, and nothing else.
pub fn root(digest: &[u8], index: i64) -> Vec<u8> {
    let mut i = W::default();
    i.bytes(1, digest);
    i.int64(2, index);
    let mut w = W::default();
    w.message(1, i);
    w.0
}

/// `sha256:` and the hex of `b`'s SHA-256: an op's name, by its bytes.
pub fn digest(b: &[u8]) -> Vec<u8> {
    let mut out = b"sha256:".to_vec();
    for byte in sha2::Sha256::digest(b) {
        out.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    out
}

/// A file the definition's steps are written in (`pb.SourceInfo`): its name, language and
/// content, and the definition (`pb.Definition`'s bytes) that loads it.
#[derive(Debug, Clone, Copy)]
pub struct SourceInfo<'a> {
    pub filename: &'a [u8],
    pub language: &'a [u8],
    pub data: &'a [u8],
    pub definition: Option<&'a [u8]>,
}

/// What a definition carries beside its ops.
#[derive(Debug, Clone, Copy, Default)]
pub struct Carried<'a> {
    /// The file its ops' locations are in, the source map's one source.
    pub source: Option<SourceInfo<'a>>,
    /// Whether the capabilities it is marshalled with have `exec.meta.setsdefaultpath`
    /// (crate::caps::op).
    pub sets_default_path: bool,
    /// Said before each progress group's number to make its ID, which BuildKit draws at
    /// random: unique to the build.
    pub group_prefix: &'a str,
}

/// A definition as written: its bytes, each op's digest, in the definition's order, and
/// the root's (empty for a definition of nothing), and every capability its metadata
/// names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marshalled {
    pub bytes: Vec<u8>,
    pub digests: Vec<Vec<u8>>,
    pub root: Vec<u8>,
    pub caps: std::collections::BTreeSet<&'static str>,
}

/// `def` as `llb.Definition.ToPB` holds it (`pb.Definition`), written as protobuf-go's
/// deterministic marshal writes it (BuildKit's own client writes its maps in Go's map
/// order, which nothing reads): each op's bytes, the root last, which names the result;
/// each op's metadata by its digest, with the capabilities `client/llb` records
/// (crate::caps); and the source map: each op's locations, empty for an op that has none
/// and none for the root, and the file they are in where any op has one
/// (`sourceMapCollector`). A definition of nothing (scratch) is empty.
pub fn definition(def: &Definition, carried: &Carried<'_>) -> Option<Marshalled> {
    let Some(root_input) = def.root else {
        return Some(Marshalled::default());
    };
    let mut ops: Vec<Vec<u8>> = Vec::with_capacity(def.ops.len() + 1);
    let mut digests: Vec<Vec<u8>> = Vec::with_capacity(def.ops.len());
    for o in &def.ops {
        let inputs: Vec<Vec<u8>> = o
            .inputs
            .iter()
            .map(|i| digests.get(i.op).cloned())
            .collect::<Option<_>>()?;
        let bytes = op(o, &inputs)?;
        digests.push(digest(&bytes));
        ops.push(bytes);
    }
    let root_op = root(digests.get(root_input.op)?, root_input.index);
    let root_digest = digest(&root_op);
    ops.push(root_op);
    let mut w = W::default();
    for o in &ops {
        w.always(1, o);
    }
    // map<string, OpMetadata>, by digest.
    let mut metadata: Vec<(&[u8], W)> = Vec::with_capacity(ops.len());
    let mut all_caps = std::collections::BTreeSet::new();
    for (i, (o, md)) in def.ops.iter().zip(&def.metadata).enumerate() {
        let caps = crate::caps::op(o, md, carried.sets_default_path);
        metadata.push((digests.get(i)?, op_metadata(md, &caps, carried.group_prefix)));
        all_caps.extend(caps);
    }
    let root_caps = crate::caps::root(&def.metadata);
    metadata.push((&root_digest, op_metadata(&Meta::default(), &root_caps, "")));
    all_caps.extend(root_caps);
    metadata.sort_by(|a, b| a.0.cmp(b.0));
    // A digest named twice would be one op twice, which the marshal never makes.
    metadata.dedup_by(|a, b| a.0 == b.0);
    for (d, m) in metadata {
        let mut entry = W::default();
        entry.always(1, d);
        entry.message(2, m);
        w.message(2, entry);
    }
    let mut source = W::default();
    let mut locations: Vec<(&[u8], &Meta)> = digests.iter().map(Vec::as_slice).zip(&def.metadata).collect();
    locations.sort_by(|a, b| a.0.cmp(b.0));
    locations.dedup_by(|a, b| a.0 == b.0);
    for (d, md) in &locations {
        let mut ls = W::default();
        for l in &md.locations {
            let mut loc = W::default();
            // Every location is in the one source: index 0, left out.
            for &(start, end) in l {
                let mut range = W::default();
                range.message(1, position(start));
                range.message(2, position(end));
                loc.message(2, range);
            }
            ls.message(1, loc);
        }
        let mut entry = W::default();
        entry.always(1, d);
        entry.message(2, ls);
        source.message(1, entry);
    }
    if let Some(info) = carried.source
        && def.metadata.iter().any(|m| !m.locations.is_empty())
    {
        source.always(2, &source_info(&info));
    }
    w.message(3, source);
    Some(Marshalled {
        bytes: w.0,
        digests,
        root: root_digest,
        caps: all_caps,
    })
}

/// A definition of a source map's own (no source of its own, as dockerui's loads of the
/// Dockerfile and .dockerignore are) as encoding/json writes the pb.Definition
/// `llb.Definition.ToPB` makes, BuildKit's errdefs.Source details carrying it so: fields
/// in the struct's order and by their JSON tags, empty ones left out, maps by key, bytes
/// in base64.
pub fn definition_json(def: &Definition, carried: &Carried<'_>) -> Option<String> {
    use base64::Engine as _;
    if carried.source.is_some() {
        return None;
    }
    let marshalled = definition(def, carried)?;
    let mut out = String::from("{");
    if def.root.is_none() {
        out.push('}');
        return Some(out);
    }
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let str_json = |s: &[u8]| {
        let mut o = String::new();
        crate::json::write_string(&mut o, s);
        o
    };
    // Each op's bytes, again, as `definition` wrote them, the root last.
    let mut ops: Vec<String> = Vec::with_capacity(def.ops.len() + 1);
    for o in &def.ops {
        let inputs: Vec<Vec<u8>> = o
            .inputs
            .iter()
            .map(|i| marshalled.digests.get(i.op).cloned())
            .collect::<Option<_>>()?;
        ops.push(format!("\"{}\"", b64(&op(o, &inputs)?)));
    }
    let root_input = def.root?;
    ops.push(format!(
        "\"{}\"",
        b64(&root(marshalled.digests.get(root_input.op)?, root_input.index))
    ));
    out.push_str(&format!("\"def\":[{}]", ops.join(",")));
    let mut metadata: Vec<(Vec<u8>, String)> = Vec::new();
    for ((o, md), d) in def.ops.iter().zip(&def.metadata).zip(&marshalled.digests) {
        let caps = crate::caps::op(o, md, carried.sets_default_path);
        metadata.push((d.clone(), op_metadata_json(md, &caps, carried.group_prefix)));
    }
    metadata.push((
        marshalled.root.clone(),
        op_metadata_json(&Meta::default(), &crate::caps::root(&def.metadata), ""),
    ));
    metadata.sort_by(|a, b| a.0.cmp(&b.0));
    let entries: Vec<String> = metadata
        .iter()
        .map(|(d, m)| format!("{}:{m}", str_json(d)))
        .collect();
    out.push_str(&format!(",\"metadata\":{{{}}}", entries.join(",")));
    let mut locations: Vec<(Vec<u8>, String)> = Vec::new();
    for (md, d) in def.metadata.iter().zip(&marshalled.digests) {
        let locs: Vec<String> = md
            .locations
            .iter()
            .map(|l| {
                let ranges: Vec<String> = l
                    .iter()
                    .map(|&(a, b)| format!("{{\"start\":{},\"end\":{}}}", position_json(a), position_json(b)))
                    .collect();
                if ranges.is_empty() {
                    "{}".to_string()
                } else {
                    format!("{{\"ranges\":[{}]}}", ranges.join(","))
                }
            })
            .collect();
        let entry = if locs.is_empty() {
            "{}".to_string()
        } else {
            format!("{{\"locations\":[{}]}}", locs.join(","))
        };
        locations.push((d.clone(), entry));
    }
    locations.sort_by(|a, b| a.0.cmp(&b.0));
    locations.dedup_by(|a, b| a.0 == b.0);
    let entries: Vec<String> = locations
        .iter()
        .map(|(d, l)| format!("{}:{l}", str_json(d)))
        .collect();
    out.push_str(&format!(
        ",\"Source\":{{\"locations\":{{{}}}}}}}",
        entries.join(",")
    ));
    Some(out)
}

/// BuildKit's errdefs.Source error detail (type URL
/// `github.com/moby/buildkit/errdefs.Source+json`) as encoding/json writes it: the file,
/// its language and content, the definition that loads it (as [`definition_json`] writes
/// it), and the lines `ranges` names, from which the client prints the excerpt.
pub fn source_json(info: &SourceInfo<'_>, definition: &str, ranges: &[(usize, usize)]) -> String {
    use base64::Engine as _;
    let s = |b: &[u8]| {
        let mut o = String::new();
        crate::json::write_string(&mut o, b);
        o
    };
    let mut fields: Vec<String> = Vec::new();
    if !info.filename.is_empty() {
        fields.push(format!("\"filename\":{}", s(info.filename)));
    }
    if !info.data.is_empty() {
        fields.push(format!(
            "\"data\":\"{}\"",
            base64::engine::general_purpose::STANDARD.encode(info.data)
        ));
    }
    fields.push(format!("\"definition\":{definition}"));
    if !info.language.is_empty() {
        fields.push(format!("\"language\":{}", s(info.language)));
    }
    let ranges: Vec<String> = ranges
        .iter()
        .map(|&(a, b)| format!("{{\"start\":{},\"end\":{}}}", position_json(a), position_json(b)))
        .collect();
    let mut out = format!("{{\"info\":{{{}}}", fields.join(","));
    if !ranges.is_empty() {
        out.push_str(&format!(",\"ranges\":[{}]", ranges.join(",")));
    }
    out.push('}');
    out
}

/// A pb.Position of a line, as encoding/json writes it: its character 0, left out.
fn position_json(line: usize) -> String {
    if line == 0 {
        "{}".to_string()
    } else {
        format!("{{\"line\":{line}}}")
    }
}

/// pb.OpMetadata as encoding/json writes it (JSON tags ignore_cache, description,
/// export_cache, caps, progress_group, linux_resources).
fn op_metadata_json(md: &Meta, caps: &std::collections::BTreeSet<&'static str>, prefix: &str) -> String {
    let s = |b: &[u8]| {
        let mut o = String::new();
        crate::json::write_string(&mut o, b);
        o
    };
    let mut fields: Vec<String> = Vec::new();
    if md.ignore_cache {
        fields.push("\"ignore_cache\":true".to_string());
    }
    if !md.description.is_empty() {
        let d: Vec<String> = md
            .description
            .iter()
            .map(|(k, v)| format!("{}:{}", s(k), s(v)))
            .collect();
        fields.push(format!("\"description\":{{{}}}", d.join(",")));
    }
    if !caps.is_empty() {
        let c: Vec<String> = caps.iter().map(|c| format!("{}:true", s(c.as_bytes()))).collect();
        fields.push(format!("\"caps\":{{{}}}", c.join(",")));
    }
    if let Some(g) = &md.progress_group {
        let mut p: Vec<String> = Vec::new();
        p.push(format!("\"id\":{}", s(format!("{prefix}{}", g.id).as_bytes())));
        if !g.name.is_empty() {
            p.push(format!("\"name\":{}", s(&g.name)));
        }
        if g.weak {
            p.push("\"weak\":true".to_string());
        }
        fields.push(format!("\"progress_group\":{{{}}}", p.join(",")));
    }
    if let Some(r) = &md.linux_resources {
        let mut l: Vec<String> = Vec::new();
        if r.memory != 0 {
            l.push(format!("\"memory\":{}", r.memory));
        }
        if r.memory_swap != 0 {
            l.push(format!("\"memorySwap\":{}", r.memory_swap));
        }
        if r.cpu_shares != 0 {
            l.push(format!("\"cpuShares\":{}", r.cpu_shares));
        }
        if r.cpu_period != 0 {
            l.push(format!("\"cpuPeriod\":{}", r.cpu_period));
        }
        if r.cpu_quota != 0 {
            l.push(format!("\"cpuQuota\":{}", r.cpu_quota));
        }
        if !r.cpuset_cpus.is_empty() {
            l.push(format!("\"cpusetCpus\":{}", s(&r.cpuset_cpus)));
        }
        if !r.cpuset_mems.is_empty() {
            l.push(format!("\"cpusetMems\":{}", s(&r.cpuset_mems)));
        }
        fields.push(format!("\"linux_resources\":{{{}}}", l.join(",")));
    }
    format!("{{{}}}", fields.join(","))
}

/// `info` as pb.SourceInfo, protobuf: a warning's source.
pub fn source_info(info: &SourceInfo<'_>) -> Vec<u8> {
    let mut i = W::default();
    i.bytes(1, info.filename);
    i.bytes(2, info.data);
    if let Some(d) = info.definition {
        i.always(3, d);
    }
    i.bytes(4, info.language);
    i.0
}

/// Lines `start` to `end` as pb.Range, protobuf, their characters 0.
pub fn range(start: usize, end: usize) -> Vec<u8> {
    let mut r = W::default();
    r.message(1, position(start));
    r.message(2, position(end));
    r.0
}

/// A line of the source as `pb.Position` has it, its character 0.
fn position(line: usize) -> W {
    let mut p = W::default();
    p.int32(1, i32::try_from(line).unwrap_or(i32::MAX));
    p
}

/// `pb.OpMetadata`: whether the cache is ignored, the description, the capabilities, the
/// progress group (its ID `prefix` and its number) and the step's limits.
fn op_metadata(md: &Meta, caps: &std::collections::BTreeSet<&'static str>, prefix: &str) -> W {
    let mut w = W::default();
    w.bool(1, md.ignore_cache);
    for (k, v) in &md.description {
        let mut entry = W::default();
        entry.always(1, k);
        entry.always(2, v);
        w.message(2, entry);
    }
    // A set's order is its keys'.
    for c in caps {
        let mut entry = W::default();
        entry.always(1, c.as_bytes());
        entry.tag(2, VARINT);
        entry.varint(1);
        w.message(5, entry);
    }
    if let Some(g) = &md.progress_group {
        let mut p = W::default();
        p.bytes(1, format!("{prefix}{}", g.id).as_bytes());
        p.bytes(2, &g.name);
        p.bool(3, g.weak);
        w.message(6, p);
    }
    if let Some(r) = &md.linux_resources {
        w.message(7, linux_resources(r));
    }
    w
}

fn linux_resources(r: &LinuxResources) -> W {
    let mut w = W::default();
    w.int64(1, r.memory);
    w.int64(2, r.memory_swap);
    w.uint(3, r.cpu_shares);
    w.uint(4, r.cpu_period);
    w.int64(5, r.cpu_quota);
    w.bytes(6, &r.cpuset_cpus);
    w.bytes(7, &r.cpuset_mems);
    w
}

fn platform(p: &Platform) -> W {
    let mut w = W::default();
    w.bytes(1, &p.architecture);
    w.bytes(2, &p.os);
    w.bytes(3, &p.variant);
    w.bytes(4, &p.os_version);
    for f in &p.os_features {
        w.always(5, f);
    }
    w
}

fn meta(p: &Process) -> W {
    let mut w = W::default();
    for a in &p.args {
        w.always(1, a);
    }
    for e in &p.env {
        w.always(2, e);
    }
    w.bytes(3, &p.cwd);
    w.bytes(4, &p.user);
    if let Some(proxy) = &p.proxy {
        let mut x = W::default();
        x.bytes(1, &proxy.http);
        x.bytes(2, &proxy.https);
        x.bytes(3, &proxy.ftp);
        x.bytes(4, &proxy.no);
        x.bytes(5, &proxy.all);
        w.message(5, x);
    }
    for h in &p.extra_hosts {
        let mut x = W::default();
        x.bytes(1, &h.host);
        x.bytes(2, &h.ip);
        w.message(6, x);
    }
    w.bytes(7, &p.hostname);
    for u in &p.ulimits {
        let mut x = W::default();
        x.bytes(1, &u.name);
        x.int64(2, u.soft);
        x.int64(3, u.hard);
        w.message(9, x);
    }
    w.bytes(10, &p.cgroup_parent);
    // The frontend's every command removes its mount stubs recursively.
    w.bool(11, true);
    w
}

fn mount(m: &OpMount) -> W {
    let mut w = W::default();
    w.int64(1, m.input);
    w.bytes(2, &m.selector);
    w.bytes(3, &m.dest);
    w.int64(4, m.output);
    w.bool(5, m.readonly);
    let kind = match &m.kind {
        OpMountKind::Bind => 0,
        OpMountKind::Secret { .. } => 1,
        OpMountKind::Ssh { .. } => 2,
        OpMountKind::Cache { .. } => 3,
        OpMountKind::Tmpfs { .. } => 4,
    };
    w.uint(6, kind);
    match &m.kind {
        OpMountKind::Bind => {}
        OpMountKind::Tmpfs { size } => {
            let mut t = W::default();
            t.int64(1, *size);
            w.message(19, t);
        }
        OpMountKind::Cache { id, sharing } => {
            let mut c = W::default();
            c.bytes(1, id);
            c.uint(
                2,
                match sharing {
                    Sharing::Shared => 0,
                    Sharing::Private => 1,
                    Sharing::Locked => 2,
                },
            );
            w.message(20, c);
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
            let mut s = W::default();
            s.bytes(1, id);
            s.uint(2, u64::from(*uid));
            s.uint(3, u64::from(*gid));
            s.uint(4, u64::from(*mode));
            s.bool(5, *optional);
            w.message(if kind == 1 { 21 } else { 22 }, s);
        }
    }
    w
}

fn device(d: &Device) -> W {
    let mut w = W::default();
    w.bytes(1, &d.name);
    w.bool(2, d.optional);
    w
}

fn action(a: &OpAction) -> W {
    let mut w = W::default();
    w.int64(1, a.input);
    w.int64(2, a.secondary_input);
    w.int64(3, a.output);
    match &a.action {
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
            let mut c = W::default();
            c.bytes(1, src);
            c.bytes(2, dest);
            if let Some(o) = owner {
                c.message(3, chown(o));
            }
            c.int32(4, *mode);
            c.bool(5, *follow_symlink);
            c.bool(6, *dir_copy_contents);
            c.bool(7, *attempt_unpack);
            c.bool(8, *create_dest_path);
            c.bool(9, *allow_wildcard);
            c.bool(10, *allow_empty_wildcard);
            c.int64(11, *timestamp);
            for p in include_patterns {
                c.always(12, p);
            }
            for p in exclude_patterns {
                c.always(13, p);
            }
            c.bytes(15, mode_str);
            for p in required_paths {
                c.always(16, p);
            }
            w.message(4, c);
        }
        OpActionKind::Mkfile {
            path,
            mode,
            data,
            owner,
            timestamp,
        } => {
            let mut f = W::default();
            f.bytes(1, path);
            f.int32(2, *mode);
            f.bytes(3, data);
            if let Some(o) = owner {
                f.message(4, chown(o));
            }
            f.int64(5, *timestamp);
            w.message(5, f);
        }
        OpActionKind::Mkdir {
            path,
            mode,
            make_parents,
            owner,
            timestamp,
        } => {
            let mut d = W::default();
            d.bytes(1, path);
            d.int32(2, *mode);
            d.bool(3, *make_parents);
            if let Some(o) = owner {
                d.message(4, chown(o));
            }
            d.int64(5, *timestamp);
            w.message(6, d);
        }
    }
    w
}

fn chown(o: &OpChown) -> W {
    let mut w = W::default();
    if let Some(u) = &o.user {
        w.message(1, user(u));
    }
    if let Some(g) = &o.group {
        w.message(2, user(g));
    }
    w
}

fn user(u: &OpUser) -> W {
    let mut w = W::default();
    match u {
        OpUser::Name { name, input } => {
            let mut n = W::default();
            n.bytes(1, name);
            n.int64(2, *input);
            w.message(1, n);
        }
        // A oneof's member is written though it is zero.
        OpUser::Id(id) => {
            w.tag(2, VARINT);
            w.varint(u64::from(*id));
        }
    }
    w
}
