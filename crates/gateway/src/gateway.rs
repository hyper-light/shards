//! The LLBBridge's calls a frontend makes (frontend/gateway/pb/gateway.proto, BuildKit
//! v0.28.1), their messages written and read as protobuf-go writes and reads them, with
//! ops.proto's Platform and SourceOp, fsutil's Stat, apicaps' PBCap, worker.proto's
//! WorkerRecord and google.rpc.Status.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use crate::grpc::{self, Error};
use crate::h2;
use crate::wire::{Reader, Value, Writer};

/// The service's path prefix.
const SERVICE: &str = "/moby.buildkit.v1.frontend.LLBBridge/";

fn proto(e: crate::wire::Error) -> Error {
    Error::Transport(e.to_string())
}

/// fsutil's Stat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stat {
    pub path: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: i64,
    pub mod_time: i64,
    pub linkname: String,
    pub devmajor: i64,
    pub devminor: i64,
    pub xattrs: BTreeMap<String, Vec<u8>>,
}

impl Stat {
    fn read(b: &[u8]) -> Result<Stat, crate::wire::Error> {
        let mut s = Stat::default();
        for f in Reader::new(b) {
            match f? {
                (1, v) => s.path = v.string()?,
                (2, v) => s.mode = v.varint()? as u32,
                (3, v) => s.uid = v.varint()? as u32,
                (4, v) => s.gid = v.varint()? as u32,
                (5, v) => s.size = v.varint()? as i64,
                (6, v) => s.mod_time = v.varint()? as i64,
                (7, v) => s.linkname = v.string()?,
                (8, v) => s.devmajor = v.varint()? as i64,
                (9, v) => s.devminor = v.varint()? as i64,
                (10, v) => {
                    let (k, v) = entry(v.bytes()?)?;
                    s.xattrs.insert(String::from_utf8_lossy(&k).into_owned(), v);
                }
                _ => {}
            }
        }
        Ok(s)
    }

    /// Whether it is a directory (Go's os.ModeDir, fsutil's mode bits).
    pub fn is_dir(&self) -> bool {
        self.mode & (1 << 31) != 0
    }
}

/// A map entry's key (field 1) and value (field 2), as bytes.
fn entry(b: &[u8]) -> Result<(Vec<u8>, Vec<u8>), crate::wire::Error> {
    let (mut k, mut v) = (Vec::new(), Vec::new());
    for f in Reader::new(b) {
        match f? {
            (1, Value::Bytes(b)) => k = b.to_vec(),
            (2, Value::Bytes(b)) => v = b.to_vec(),
            _ => {}
        }
    }
    Ok((k, v))
}

/// ops.proto's Platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    pub variant: String,
    pub os_version: String,
    pub os_features: Vec<String>,
}

impl Platform {
    fn write(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.string(1, &self.architecture);
        w.string(2, &self.os);
        w.string(3, &self.variant);
        w.string(4, &self.os_version);
        for f in &self.os_features {
            w.message(5, f.as_bytes());
        }
        w.0
    }

    fn read(b: &[u8]) -> Result<Platform, crate::wire::Error> {
        let mut p = Platform::default();
        for f in Reader::new(b) {
            match f? {
                (1, v) => p.architecture = v.string()?,
                (2, v) => p.os = v.string()?,
                (3, v) => p.variant = v.string()?,
                (4, v) => p.os_version = v.string()?,
                (5, v) => p.os_features.push(v.string()?),
                _ => {}
            }
        }
        Ok(p)
    }
}

/// A capability BuildKit says it has (apicaps.PBCap): enabled, or disabled with the
/// message it gives the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cap {
    pub id: String,
    pub enabled: bool,
    pub disabled_reason_msg: String,
}

fn caps(b: &[u8]) -> Result<Cap, crate::wire::Error> {
    let mut c = Cap::default();
    for f in Reader::new(b) {
        match f? {
            (1, v) => c.id = v.string()?,
            (2, v) => c.enabled = v.varint()? != 0,
            (5, v) => c.disabled_reason_msg = v.string()?,
            _ => {}
        }
    }
    Ok(c)
}

/// A worker BuildKit offers (WorkerRecord): its ID, labels and platforms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worker {
    pub id: String,
    pub labels: BTreeMap<String, String>,
    pub platforms: Vec<Platform>,
}

/// What Ping answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pong {
    pub frontend_caps: Vec<Cap>,
    pub llb_caps: Vec<Cap>,
    pub workers: Vec<Worker>,
}

impl Pong {
    /// Whether BuildKit says it has frontend capability `id`, enabled.
    pub fn has(&self, id: &str) -> bool {
        self.frontend_caps.iter().any(|c| c.id == id && c.enabled)
    }
}

/// A result's reference: its ID, and the definition it was solved from (pb.Definition).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ref {
    pub id: String,
    pub def: Vec<u8>,
}

impl Ref {
    fn write(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.string(1, &self.id);
        if !self.def.is_empty() {
            w.message(2, &self.def);
        }
        w.0
    }

    fn read(b: &[u8]) -> Result<Ref, crate::wire::Error> {
        let mut r = Ref::default();
        for f in Reader::new(b) {
            match f? {
                (1, v) => r.id = v.string()?,
                (2, v) => r.def = v.bytes()?.to_vec(),
                _ => {}
            }
        }
        Ok(r)
    }
}

/// What Solve is asked (SolveRequest), the fields a frontend sets.
#[derive(Debug, Clone, Default)]
pub struct Solve<'a> {
    /// pb.Definition, as protobuf; none to solve another frontend's.
    pub definition: Option<&'a [u8]>,
    pub frontend: &'a str,
    pub frontend_opt: BTreeMap<String, String>,
    /// Caches to import (CacheOptionsEntry): each its type and attributes.
    pub cache_imports: Vec<(String, BTreeMap<String, String>)>,
    pub frontend_inputs: BTreeMap<String, Vec<u8>>,
    pub evaluate: bool,
}

/// What a solve answered: its result (gateway.proto's Result) as BuildKit wrote it, and
/// the reference it holds where it holds one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Solved {
    pub result: Vec<u8>,
    pub single: Option<Ref>,
}

/// An image's metadata, as ResolveSourceMeta and ResolveImageConfig answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageMeta {
    pub digest: String,
    pub config: Vec<u8>,
}

/// A Git source's metadata (ResolveSourceGitResponse): what it resolved to, and the
/// commit's and tag's objects where they were asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitMeta {
    pub checksum: String,
    pub reference: String,
    pub commit_checksum: String,
    pub commit_object: Vec<u8>,
    pub tag_object: Vec<u8>,
}

/// An HTTP source's metadata (ResolveSourceHTTPResponse): its checksum, file name, and
/// when it was last modified (seconds and nanoseconds since 1970), if it says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpMeta {
    pub checksum: String,
    pub filename: String,
    pub last_modified: Option<(i64, i32)>,
}

/// What ResolveSourceMeta answers: the source (perhaps converted by a policy) and its
/// metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMeta {
    pub identifier: String,
    pub attrs: BTreeMap<String, String>,
    pub image: Option<ImageMeta>,
    pub git: Option<GitMeta>,
    pub http: Option<HttpMeta>,
}

/// What a frontend returns (Result): its reference, or one for each platform, and the
/// metadata BuildKit's exporters read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Returned {
    pub single: Option<Ref>,
    pub refs: BTreeMap<String, Ref>,
    pub metadata: BTreeMap<String, Vec<u8>>,
}

/// A warning (WarnRequest): the step it is on, its level, its line and detail lines, its
/// documentation's URL, and where it is in a source: `info` (pb.SourceInfo, as protobuf)
/// and its `ranges` (pb.Range each).
#[derive(Debug, Clone, Copy, Default)]
pub struct Warning<'a> {
    pub digest: &'a str,
    pub level: i64,
    pub short: &'a [u8],
    pub detail: &'a [Vec<u8>],
    pub url: &'a str,
    pub info: Option<&'a [u8]>,
    pub ranges: &'a [Vec<u8>],
}

/// google.rpc.Status: a code, its message, and details (Any: type URL and value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RpcStatus {
    pub code: i32,
    pub message: String,
    pub details: Vec<(String, Vec<u8>)>,
}

impl RpcStatus {
    pub fn write(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.int32(1, self.code);
        w.string(2, &self.message);
        for (url, value) in &self.details {
            let mut any = Writer::default();
            any.string(1, url);
            any.bytes(2, value);
            w.message(3, &any.0);
        }
        w.0
    }

    pub fn read(b: &[u8]) -> Result<RpcStatus, crate::wire::Error> {
        let mut s = RpcStatus::default();
        for f in Reader::new(b) {
            match f? {
                (1, v) => s.code = v.varint()? as i32,
                (2, v) => s.message = v.string()?,
                (3, v) => {
                    let (url, value) = entry(v.bytes()?)?;
                    s.details
                        .push((String::from_utf8_lossy(&url).into_owned(), value));
                }
                _ => {}
            }
        }
        Ok(s)
    }
}

/// A frontend's client of the gateway BuildKit gave it.
#[derive(Debug)]
pub struct Client<R, W> {
    conn: h2::Conn<R, W>,
}

impl<R: Read, W: Write> Client<R, W> {
    pub fn new(r: R, w: W) -> Result<Client<R, W>, Error> {
        Ok(Client {
            conn: h2::Conn::new(r, w)?,
        })
    }

    fn call(&mut self, method: &str, request: &[u8]) -> Result<Vec<u8>, Error> {
        grpc::call(&mut self.conn, &format!("{SERVICE}{method}"), request)
    }

    pub fn ping(&mut self) -> Result<Pong, Error> {
        let b = self.call("Ping", &[])?;
        let mut pong = Pong::default();
        for f in Reader::new(&b) {
            match f.map_err(proto)? {
                (1, v) => pong
                    .frontend_caps
                    .push(caps(v.bytes().map_err(proto)?).map_err(proto)?),
                (2, v) => pong
                    .llb_caps
                    .push(caps(v.bytes().map_err(proto)?).map_err(proto)?),
                (3, v) => {
                    let mut worker = Worker::default();
                    for wf in Reader::new(v.bytes().map_err(proto)?) {
                        match wf.map_err(proto)? {
                            (1, v) => worker.id = v.string().map_err(proto)?,
                            (2, v) => {
                                let (k, v) = entry(v.bytes().map_err(proto)?).map_err(proto)?;
                                worker.labels.insert(
                                    String::from_utf8_lossy(&k).into_owned(),
                                    String::from_utf8_lossy(&v).into_owned(),
                                );
                            }
                            (3, v) => worker
                                .platforms
                                .push(Platform::read(v.bytes().map_err(proto)?).map_err(proto)?),
                            _ => {}
                        }
                    }
                    pong.workers.push(worker);
                }
                _ => {}
            }
        }
        Ok(pong)
    }

    /// The frontend's inputs: each a pb.Definition, by name.
    pub fn inputs(&mut self) -> Result<BTreeMap<String, Vec<u8>>, Error> {
        let b = self.call("Inputs", &[])?;
        let mut out = BTreeMap::new();
        for f in Reader::new(&b) {
            if let (1, v) = f.map_err(proto)? {
                let (k, v) = entry(v.bytes().map_err(proto)?).map_err(proto)?;
                out.insert(String::from_utf8_lossy(&k).into_owned(), v);
            }
        }
        Ok(out)
    }

    /// A definition or another frontend's build solved: its result, and the reference it
    /// holds where it holds one.
    pub fn solve(&mut self, s: &Solve<'_>) -> Result<Solved, Error> {
        let mut w = Writer::default();
        if let Some(def) = s.definition {
            w.message(1, def);
        }
        w.string(2, s.frontend);
        w.map(3, s.frontend_opt.iter().map(|(k, v)| (k.as_str(), v)), |e, v| {
            e.message(2, v.as_bytes())
        });
        // allowResultReturn and allowResultArrayRef, as grpcclient always sets them.
        w.bool(5, true);
        w.bool(6, true);
        for (kind, attrs) in &s.cache_imports {
            let mut c = Writer::default();
            c.string(1, kind);
            c.map(2, attrs.iter().map(|(k, v)| (k.as_str(), v)), |e, v| {
                e.message(2, v.as_bytes())
            });
            w.message(12, &c.0);
        }
        w.map(
            13,
            s.frontend_inputs.iter().map(|(k, v)| (k.as_str(), v)),
            |e, v| {
                e.message(2, v);
            },
        );
        w.bool(14, s.evaluate);
        let b = self.call("Solve", &w.0)?;
        // SolveResponse.result (3), its .ref (3) if it holds one.
        let mut out = Solved::default();
        for f in Reader::new(&b) {
            if let (3, v) = f.map_err(proto)? {
                let result = v.bytes().map_err(proto)?;
                out.result = result.to_vec();
                for rf in Reader::new(result) {
                    if let (3, v) = rf.map_err(proto)? {
                        out.single = Some(Ref::read(v.bytes().map_err(proto)?).map_err(proto)?);
                    }
                }
            }
        }
        Ok(out)
    }

    /// `path` of reference `id`, its `range` (offset, length) where given.
    pub fn read_file(&mut self, id: &str, path: &str, range: Option<(i64, i64)>) -> Result<Vec<u8>, Error> {
        let mut w = Writer::default();
        w.string(1, id);
        w.string(2, path);
        if let Some((offset, length)) = range {
            let mut r = Writer::default();
            r.int64(1, offset);
            r.int64(2, length);
            w.message(3, &r.0);
        }
        let b = self.call("ReadFile", &w.0)?;
        for f in Reader::new(&b) {
            if let (1, v) = f.map_err(proto)? {
                return Ok(v.bytes().map_err(proto)?.to_vec());
            }
        }
        Ok(Vec::new())
    }

    pub fn stat_file(&mut self, id: &str, path: &str) -> Result<Stat, Error> {
        let mut w = Writer::default();
        w.string(1, id);
        w.string(2, path);
        let b = self.call("StatFile", &w.0)?;
        for f in Reader::new(&b) {
            if let (1, v) = f.map_err(proto)? {
                return Stat::read(v.bytes().map_err(proto)?).map_err(proto);
            }
        }
        Ok(Stat::default())
    }

    /// The entries of directory `path` of reference `id`, those `include` matches where
    /// it is given.
    pub fn read_dir(&mut self, id: &str, path: &str, include: &str) -> Result<Vec<Stat>, Error> {
        let mut w = Writer::default();
        w.string(1, id);
        w.string(2, path);
        w.string(3, include);
        let b = self.call("ReadDir", &w.0)?;
        let mut out = Vec::new();
        for f in Reader::new(&b) {
            if let (1, v) = f.map_err(proto)? {
                out.push(Stat::read(v.bytes().map_err(proto)?).map_err(proto)?);
            }
        }
        Ok(out)
    }

    /// A source's metadata (an image's config and digest, a Git source's commit, an HTTP
    /// source's checksum and time), resolved for `platform` and shown as the step
    /// `log_name`; a Git source's commit and tag objects where `git_objects`.
    pub fn resolve_source_meta(
        &mut self,
        identifier: &str,
        attrs: &BTreeMap<String, String>,
        platform: Option<&Platform>,
        log_name: &str,
        resolve_mode: &str,
        git_objects: bool,
    ) -> Result<SourceMeta, Error> {
        let mut source = Writer::default();
        source.string(1, identifier);
        source.map(2, attrs.iter().map(|(k, v)| (k.as_str(), v)), |e, v| {
            e.message(2, v.as_bytes())
        });
        let mut w = Writer::default();
        w.message(1, &source.0);
        if let Some(p) = platform {
            w.message(2, &p.write());
        }
        w.string(3, log_name);
        w.string(4, resolve_mode);
        if git_objects {
            let mut g = Writer::default();
            g.bool(1, true);
            w.message(5, &g.0);
        }
        let b = self.call("ResolveSourceMeta", &w.0)?;
        let mut out = SourceMeta::default();
        for f in Reader::new(&b) {
            match f.map_err(proto)? {
                (1, v) => {
                    for sf in Reader::new(v.bytes().map_err(proto)?) {
                        match sf.map_err(proto)? {
                            (1, v) => out.identifier = v.string().map_err(proto)?,
                            (2, v) => {
                                let (k, v) = entry(v.bytes().map_err(proto)?).map_err(proto)?;
                                out.attrs.insert(
                                    String::from_utf8_lossy(&k).into_owned(),
                                    String::from_utf8_lossy(&v).into_owned(),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                (2, v) => {
                    let mut image = ImageMeta::default();
                    for imf in Reader::new(v.bytes().map_err(proto)?) {
                        match imf.map_err(proto)? {
                            (1, v) => image.digest = v.string().map_err(proto)?,
                            (2, v) => image.config = v.bytes().map_err(proto)?.to_vec(),
                            _ => {}
                        }
                    }
                    out.image = Some(image);
                }
                (3, v) => {
                    let mut git = GitMeta::default();
                    for gf in Reader::new(v.bytes().map_err(proto)?) {
                        match gf.map_err(proto)? {
                            (1, v) => git.checksum = v.string().map_err(proto)?,
                            (2, v) => git.reference = v.string().map_err(proto)?,
                            (3, v) => git.commit_checksum = v.string().map_err(proto)?,
                            (4, v) => git.commit_object = v.bytes().map_err(proto)?.to_vec(),
                            (5, v) => git.tag_object = v.bytes().map_err(proto)?.to_vec(),
                            _ => {}
                        }
                    }
                    out.git = Some(git);
                }
                (4, v) => {
                    let mut http = HttpMeta::default();
                    for hf in Reader::new(v.bytes().map_err(proto)?) {
                        match hf.map_err(proto)? {
                            (1, v) => http.checksum = v.string().map_err(proto)?,
                            (2, v) => http.filename = v.string().map_err(proto)?,
                            // google.protobuf.Timestamp: seconds (1), nanos (2).
                            (3, v) => {
                                let mut t = (0i64, 0i32);
                                for tf in Reader::new(v.bytes().map_err(proto)?) {
                                    match tf.map_err(proto)? {
                                        (1, v) => t.0 = v.varint().map_err(proto)? as i64,
                                        (2, v) => t.1 = v.varint().map_err(proto)? as i32,
                                        _ => {}
                                    }
                                }
                                http.last_modified = Some(t);
                            }
                            _ => {}
                        }
                    }
                    out.http = Some(http);
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// A warning of the build's on the step `digest` names (WarnRequest).
    pub fn warn(&mut self, warning: &Warning<'_>) -> Result<(), Error> {
        let mut w = Writer::default();
        w.string(1, warning.digest);
        w.int64(2, warning.level);
        w.bytes(3, warning.short);
        for d in warning.detail {
            w.message(4, d);
        }
        w.string(5, warning.url);
        if let Some(i) = warning.info {
            w.message(6, i);
        }
        for r in warning.ranges {
            w.message(7, r);
        }
        self.call("Warn", &w.0).map(|_| ())
    }

    /// Another frontend's result, as its solve answered it (gateway.proto's Result),
    /// returned as this frontend's.
    pub fn return_raw(&mut self, result: &[u8]) -> Result<(), Error> {
        let mut w = Writer::default();
        w.message(1, result);
        self.call("Return", &w.0).map(|_| ())
    }

    /// The frontend's result, or its error, returned (ReturnRequest).
    pub fn return_result(&mut self, result: Result<&Returned, &RpcStatus>) -> Result<(), Error> {
        let mut w = Writer::default();
        match result {
            Ok(r) => {
                let mut res = Writer::default();
                if let Some(single) = &r.single {
                    res.message(3, &single.write());
                } else {
                    let mut refs = Writer::default();
                    refs.map(1, r.refs.iter().map(|(k, v)| (k.as_str(), v)), |e, v| {
                        e.message(2, &v.write())
                    });
                    res.message(4, &refs.0);
                }
                res.map(10, r.metadata.iter().map(|(k, v)| (k.as_str(), v)), |e, v| {
                    e.message(2, v)
                });
                w.message(1, &res.0);
            }
            Err(status) => w.message(2, &status.write()),
        }
        self.call("Return", &w.0).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The server's half of docker/dockerfile:1's own traffic with BuildKit v0.28.1
    /// (Docker 29.3.1; D113's spy in `shards-dind`), answering this client's calls made in
    /// that frontend's order: every response and error read as grpc-go read them.
    #[test]
    fn buildkits_own_answers_are_read_as_grpc_go_read_them() {
        let server = std::io::Cursor::new(include_bytes!("../testdata/dockerfile-1.server.bin").to_vec());
        let mut c = Client::new(server, Vec::new()).unwrap();
        let pong = c.ping().unwrap();
        for cap in [
            "frontend.caps",
            "frontend.inputs",
            "gateway.evaluate",
            "proto.refarray",
            "return",
        ] {
            assert!(pong.has(cap), "{cap}: {:?}", pong.frontend_caps);
        }
        assert_eq!(pong.workers.len(), 1);
        assert_eq!(pong.workers[0].platforms[0].os, "linux");
        assert!(c.inputs().unwrap().is_empty());
        let dockerfile = c.solve(&Solve::default()).unwrap().single.unwrap();
        assert_eq!(dockerfile.id, "z6j6yo9zwftwmksk8pkh27fgr");
        let stat = c.stat_file(&dockerfile.id, "Dockerfile").unwrap();
        assert_eq!(
            (stat.path.as_str(), stat.mode, stat.size),
            ("Dockerfile", 0o644, 91)
        );
        let text = c
            .read_file(&dockerfile.id, "Dockerfile", Some((0, 16_777_217)))
            .unwrap();
        assert!(text.starts_with(b"# syntax=127.0.0.1:15113/shards-d113-spy:1\nFROM alpine:3.20\n"));
        for (call, said) in [
            ("stat", "lstat Dockerfile.dockerignore: no such file or directory"),
            ("read", "open Dockerfile.dockerignore: no such file or directory"),
        ] {
            let e = match call {
                "stat" => c
                    .stat_file(&dockerfile.id, "Dockerfile.dockerignore")
                    .unwrap_err(),
                _ => c
                    .read_file(&dockerfile.id, "Dockerfile.dockerignore", Some((0, 16_777_217)))
                    .unwrap_err(),
            };
            let Error::Status(s) = e else { panic!("{e:?}") };
            assert_eq!((s.code, s.message.as_str()), (2, said));
            let status = RpcStatus::read(&s.details).unwrap();
            assert_eq!(status.message, said);
            assert!(
                status
                    .details
                    .iter()
                    .any(|(url, _)| url.ends_with("stack.Stack+json"))
            );
        }
        let meta = c
            .resolve_source_meta(
                "docker-image://docker.io/library/alpine:3.20",
                &BTreeMap::new(),
                None,
                "",
                "",
                false,
            )
            .unwrap();
        assert_eq!(meta.identifier, "docker-image://docker.io/library/alpine:3.20");
        let image = meta.image.unwrap();
        assert_eq!(
            image.digest,
            "sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc"
        );
        assert!(image.config.starts_with(b"{\"architecture\":\"arm64\""));
        assert_eq!(
            c.solve(&Solve::default()).unwrap().single.unwrap().id,
            "d3aoolwed5lk9e7l5gra2666o"
        );
        assert!(c.stat_file("d3aoolwed5lk9e7l5gra2666o", ".dockerignore").is_err());
        assert!(
            c.read_file("d3aoolwed5lk9e7l5gra2666o", ".dockerignore", None)
                .is_err()
        );
        let built = c.solve(&Solve::default()).unwrap().single.unwrap();
        assert_eq!(built.id, "h71kuwh4g5ikvyaafsq04qhl8");
        assert!(!built.def.is_empty());
        c.return_result(Ok(&Returned {
            single: Some(built),
            ..Returned::default()
        }))
        .unwrap();
    }
}
