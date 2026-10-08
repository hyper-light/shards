//! LLB operations as BuildKit marshals them (D80): `pb.Op` (solver/pb/ops.proto) in
//! protobuf's wire format, as protobuf-go's deterministic marshal writes it
//! (`client/llb` marshals each op so, and names it by its bytes' digest): fields in their
//! numbers' order, proto3's zero values left out, set messages written however empty,
//! repeated fields each element, and map entries by their keys' order, key and value both.
//! Its order is protobuf-go's legacy one (`order.LegacyFieldOrder`): a message's oneof
//! after its other fields, so an op's inputs, platform and constraints come before what
//! it does.
//! What provenance's `mode=max` records of a build (its LLB definition) is these.

use crate::llb::{
    Device, NetMode, Op, OpAction, OpActionKind, OpChown, OpKind, OpMount, OpMountKind, OpUser, Process,
    Security, Sharing,
};
use crate::platform::Platform;

const VARINT: u8 = 0;
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

/// `op`'s bytes, its inputs named by `digests` (each input's op's digest, in order). None
/// for shards' own steps, which BuildKit has no message for.
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
        OpKind::Skills { .. } => return None,
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
