//! eStargz layers as BuildKit v0.28.1 writes them (D82): stargz-snapshotter v0.18.2's
//! `estargz.Writer` (estargz.go, gzip.go), `NewWriterLevel` then `AppendTarLossLess` and
//! `Close`. The tar passes through unchanged, each regular file's data in 4 MiB chunks,
//! each chunk starting a gzip member of its own; then the table of contents, a tar of one
//! entry (`stargz.index.json`) in a member of its own, and a 51-byte footer naming where
//! it starts. Held to the writer's own output by `estargz_is_stargz_snapshotters`.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use sha2::{Digest as _, Sha256};
use shards_archive::tar::{
    self as gotar, Header, TYPE_BLOCK, TYPE_CHAR, TYPE_DIR, TYPE_FIFO, TYPE_LINK, TYPE_REG, TYPE_SYMLINK,
};
use shards_flate::GzipWriter;

/// The TOC's entry name (`TOCTarName`).
const TOC_NAME: &[u8] = b"stargz.index.json";
/// A regular file's data in one gzip member, at most (`chunkSize`'s default).
const CHUNK: i64 = 4 << 20;
/// The annotations a layer's descriptor carries (`EStargzAnnotations`).
pub const TOC_DIGEST: &str = "containerd.io/snapshot/stargz/toc.digest";
pub const UNCOMPRESSED_SIZE: &str = "io.containers.estargz.uncompressed-size";

/// What a layer written so is, beside its bytes: its TOC's digest, its DiffID (the tar
/// with the TOC's entry), and that tar's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub toc_digest: String,
    pub diff_id: String,
    pub uncompressed: u64,
}

/// What it reads from: the tar, its bytes kept while `recording` (`RawAccounting`).
struct Tee<R> {
    r: R,
    recording: bool,
    raw: Vec<u8>,
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.r.read(buf)?;
        if self.recording {
            self.raw.extend_from_slice(buf.get(..n).unwrap_or_default());
        }
        Ok(n)
    }
}

/// The output, and how many bytes it has taken (`countWriter`).
struct Count<O> {
    o: O,
    n: u64,
}

impl<O: Write> Write for Count<O> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.o.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.o.flush()
    }
}

/// The current gzip member, or none, holding the output between them.
enum Member<O: Write> {
    Closed(Count<O>),
    Open(Box<GzipWriter<Count<O>>>),
    Gone,
}

/// A `TOCEntry`, its fields as JSON names them, in order; zero ones left out.
#[derive(Debug, Clone, Default)]
struct Entry {
    name: Vec<u8>,
    kind: &'static str,
    size: i64,
    modtime: String,
    link_name: Vec<u8>,
    mode: i64,
    uid: i64,
    gid: i64,
    uname: Vec<u8>,
    gname: Vec<u8>,
    offset: u64,
    dev_major: i64,
    dev_minor: i64,
    xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    digest: String,
    chunk_offset: i64,
    chunk_size: i64,
    chunk_digest: String,
}

struct Writer<O: Write> {
    member: Member<O>,
    level: i32,
    diff: Sha256,
    uncompressed: u64,
    entries: Vec<Entry>,
    last_uname: BTreeMap<i64, Vec<u8>>,
    last_gname: BTreeMap<i64, Vec<u8>>,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("estargz: {e}")
}

impl<O: Write> Writer<O> {
    /// What the tar's bytes are written through (`currentCompressionWriter`): the DiffID's
    /// hash, and the current member, opened where there is none.
    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.diff.update(b);
        self.uncompressed += b.len() as u64;
        self.open()?;
        match &mut self.member {
            Member::Open(gz) => gz.write_all(b).map_err(err),
            _ => Err(err("no gzip member")),
        }
    }

    /// `condOpenGz`.
    fn open(&mut self) -> Result<(), String> {
        if let Member::Closed(_) = self.member
            && let Member::Closed(c) = std::mem::replace(&mut self.member, Member::Gone)
        {
            self.member = Member::Open(Box::new(GzipWriter::new(c, self.level).map_err(err)?));
        }
        Ok(())
    }

    /// `flushGz`: the member's pending bytes out, and a sync marker.
    fn flush(&mut self) -> Result<(), String> {
        match &mut self.member {
            Member::Open(gz) => gz.sync_flush().map_err(err),
            _ => Ok(()),
        }
    }

    /// `closeGz`.
    fn close(&mut self) -> Result<(), String> {
        if let Member::Open(_) = self.member
            && let Member::Open(gz) = std::mem::replace(&mut self.member, Member::Gone)
        {
            self.member = Member::Closed(gz.finish().map_err(err)?);
        }
        Ok(())
    }

    /// `w.cw.n`, between members.
    fn offset(&self) -> Result<u64, String> {
        match &self.member {
            Member::Closed(c) => Ok(c.n),
            _ => Err(err("an offset within a member")),
        }
    }

    /// `nameIfChanged`.
    fn name_if_changed(last: &mut BTreeMap<i64, Vec<u8>>, id: i64, name: &[u8]) -> Vec<u8> {
        if name.is_empty() || last.get(&id).is_some_and(|n| n == name) {
            return Vec::new();
        }
        last.insert(id, name.to_vec());
        name.to_vec()
    }
}

/// `formatModtime`: RFC 3339 in UTC, rounded to the second; none at Go's zero time or
/// the epoch.
fn modtime(t: gotar::Time) -> String {
    if t == gotar::Time::ZERO || t.sec == 0 {
        return String::new();
    }
    let secs = if t.nsec >= 500_000_000 {
        t.sec.wrapping_add(1)
    } else {
        t.sec
    };
    shards_dockerfile::go::Time::from_unix(secs)
        .rfc3339_nano()
        .unwrap_or_default()
}

/// `src`, a tar, written to `out` as an eStargz layer at gzip `level` (BuildKit's
/// `compression-level`, else `gzip.DefaultCompression`); what was written, and `out`.
pub fn write<R: Read, O: Write>(src: R, out: O, level: i32) -> Result<(O, Written), String> {
    let mut w = Writer {
        member: Member::Closed(Count { o: out, n: 0 }),
        level,
        diff: Sha256::new(),
        uncompressed: 0,
        entries: Vec::new(),
        last_uname: BTreeMap::new(),
        last_gname: BTreeMap::new(),
    };
    let mut tr = gotar::Reader::new(Tee {
        r: src,
        recording: false,
        raw: Vec::new(),
    });
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tr.get_mut().recording = true;
        let next = tr
            .next_header()
            .map_err(|e| format!("error reading from source tar: tar.Reader.Next: {e}"))?;
        let tee = tr.get_mut();
        tee.recording = false;
        let raw = std::mem::take(&mut tee.raw);
        let Some(h) = next else {
            // The rest of the archive as it is: its end, and anything after.
            w.write(&raw)?;
            break;
        };
        if clean_name(&h.name) == TOC_NAME {
            return Err("existing TOC JSON is not allowed; decompress layer before append".into());
        }
        let mut ent = entry(&mut w, &h)?;
        w.open()?;
        w.write(&raw)?;
        if h.typeflag != TYPE_REG {
            w.entries.push(ent);
            continue;
        }
        // A regular file's entry: its digest set once its data is all read, as Go sets it
        // through the pointer it has already listed.
        if h.size <= 0 {
            ent.digest = format!("sha256:{}", hex(&Sha256::digest([])));
            w.entries.push(ent);
            continue;
        }
        let first = w.entries.len();
        let mut payload = Sha256::new();
        let mut written: i64 = 0;
        while written < h.size {
            let remain = h.size - written;
            let chunk = if remain < CHUNK {
                remain
            } else {
                ent.chunk_size = CHUNK;
                CHUNK
            };
            // Every chunk a member of its own (MinChunkSize 0): what is pending flushed,
            // then the member ended and another begun.
            w.flush()?;
            w.close()?;
            ent.offset = w.offset()?;
            ent.chunk_offset = written;
            w.open()?;
            let mut chunk_hash = Sha256::new();
            let mut left = u64::try_from(chunk).map_err(err)?;
            while left > 0 {
                let want = usize::try_from(left.min(buf.len() as u64)).map_err(err)?;
                let part = buf.get_mut(..want).ok_or_else(|| err("a read buffer"))?;
                let n = tr.read(part).map_err(err)?;
                if n == 0 {
                    return Err(format!(
                        "error copying {:?}: EOF",
                        String::from_utf8_lossy(&h.name)
                    ));
                }
                let got = part.get(..n).unwrap_or_default();
                payload.update(got);
                chunk_hash.update(got);
                w.write(got)?;
                left -= n as u64;
            }
            ent.chunk_digest = format!("sha256:{}", hex(&chunk_hash.finalize()));
            w.entries.push(ent);
            written += chunk;
            ent = Entry {
                name: h.name.clone(),
                kind: "chunk",
                ..Entry::default()
            };
        }
        if let Some(e) = w.entries.get_mut(first) {
            e.digest = format!("sha256:{}", hex(&payload.finalize()));
        }
    }
    // Close: the last member, then the TOC in a member of its own, its tar part of the
    // DiffID's, then the footer naming where it starts.
    w.close()?;
    let toc_offset = w.offset()?;
    let toc = toc_json(&w.entries);
    let mut tar = gotar::Writer::new(Vec::new());
    tar.write_header(&Header {
        typeflag: TYPE_REG,
        name: TOC_NAME.to_vec(),
        size: i64::try_from(toc.len()).map_err(err)?,
        ..Header::default()
    })
    .map_err(err)?;
    tar.write_all(&toc).map_err(err)?;
    let tar = tar.finish().map_err(err)?;
    w.write(&tar)?;
    w.close()?;
    let Member::Closed(mut c) = std::mem::replace(&mut w.member, Member::Gone) else {
        return Err(err("no output"));
    };
    c.write_all(&footer(toc_offset)?).map_err(err)?;
    let written = Written {
        toc_digest: format!("sha256:{}", hex(&Sha256::digest(&toc))),
        diff_id: format!("sha256:{}", hex(&w.diff.finalize())),
        uncompressed: w.uncompressed,
    };
    Ok((c.o, written))
}

/// The TOC as `json.MarshalIndent(toc, "", "\t")` writes `JTOC`: its version, and its
/// entries (`null` where there are none), each field left out at its zero value.
fn toc_json(entries: &[Entry]) -> Vec<u8> {
    use base64::Engine as _;
    use shards_dockerfile::export::json_string;
    let mut out = String::from("{\"version\":1,\"entries\":");
    if entries.is_empty() {
        out.push_str("null");
    } else {
        out.push('[');
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let mut f: Vec<String> = vec![
                format!("\"name\":{}", json_string(&e.name)),
                format!("\"type\":{}", json_string(e.kind.as_bytes())),
            ];
            let mut int = |k: &str, v: i64| {
                if v != 0 {
                    f.push(format!("\"{k}\":{v}"));
                }
            };
            int("size", e.size);
            let mut f2: Vec<String> = Vec::new();
            if !e.modtime.is_empty() {
                f2.push(format!("\"modtime\":{}", json_string(e.modtime.as_bytes())));
            }
            if !e.link_name.is_empty() {
                f2.push(format!("\"linkName\":{}", json_string(&e.link_name)));
            }
            f.extend(f2);
            for (k, v) in [("mode", e.mode), ("uid", e.uid), ("gid", e.gid)] {
                if v != 0 {
                    f.push(format!("\"{k}\":{v}"));
                }
            }
            if !e.uname.is_empty() {
                f.push(format!("\"userName\":{}", json_string(&e.uname)));
            }
            if !e.gname.is_empty() {
                f.push(format!("\"groupName\":{}", json_string(&e.gname)));
            }
            for (k, v) in [
                ("offset", i64::try_from(e.offset).unwrap_or(i64::MAX)),
                ("devMajor", e.dev_major),
                ("devMinor", e.dev_minor),
            ] {
                if v != 0 {
                    f.push(format!("\"{k}\":{v}"));
                }
            }
            if !e.xattrs.is_empty() {
                let x: Vec<String> = e
                    .xattrs
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{}:{}",
                            json_string(k),
                            json_string(base64::engine::general_purpose::STANDARD.encode(v).as_bytes())
                        )
                    })
                    .collect();
                f.push(format!("\"xattrs\":{{{}}}", x.join(",")));
            }
            if !e.digest.is_empty() {
                f.push(format!("\"digest\":{}", json_string(e.digest.as_bytes())));
            }
            for (k, v) in [("chunkOffset", e.chunk_offset), ("chunkSize", e.chunk_size)] {
                if v != 0 {
                    f.push(format!("\"{k}\":{v}"));
                }
            }
            if !e.chunk_digest.is_empty() {
                f.push(format!(
                    "\"chunkDigest\":{}",
                    json_string(e.chunk_digest.as_bytes())
                ));
            }
            out.push('{');
            out.push_str(&f.join(","));
            out.push('}');
        }
        out.push(']');
    }
    out.push('}');
    shards_dockerfile::json_indent_with(out.as_bytes(), b"\t")
}

/// `gzipFooterBytes`: an empty gzip member, stored, whose extra field (`SG`, its length,
/// then the TOC's offset in 16 hex digits and `STARGZ`) names where the TOC starts.
fn footer(toc_offset: u64) -> Result<Vec<u8>, String> {
    let subfield = format!("{toc_offset:016x}STARGZ");
    let len = u16::try_from(subfield.len()).map_err(err)?;
    let mut extra = vec![b'S', b'G'];
    extra.extend_from_slice(&len.to_le_bytes());
    extra.extend_from_slice(subfield.as_bytes());
    let mut gz = GzipWriter::new(Vec::new(), shards_flate::NO_COMPRESSION).map_err(err)?;
    gz.set_extra(extra);
    let out = gz.finish().map_err(err)?;
    if out.len() != 51 {
        return Err(format!("footer buffer = {}, not 51", out.len()));
    }
    Ok(out)
}

fn entry<O: Write>(w: &mut Writer<O>, h: &Header) -> Result<Entry, String> {
    let kind = match h.typeflag {
        TYPE_LINK => "hardlink",
        TYPE_SYMLINK => "symlink",
        TYPE_DIR => "dir",
        TYPE_REG => "reg",
        TYPE_CHAR => "char",
        TYPE_BLOCK => "block",
        TYPE_FIFO => "fifo",
        other => return Err(format!("unsupported input tar entry {:?}", char::from(other))),
    };
    let xattrs = h
        .pax
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(b"SCHILY.xattr.".as_slice())
                .map(|k| (k.to_vec(), v.clone()))
        })
        .collect();
    Ok(Entry {
        name: h.name.clone(),
        kind,
        size: if kind == "reg" { h.size } else { 0 },
        modtime: modtime(h.mtime),
        link_name: if matches!(kind, "hardlink" | "symlink") {
            h.linkname.clone()
        } else {
            Vec::new()
        },
        mode: h.mode,
        uid: h.uid,
        gid: h.gid,
        uname: Writer::<O>::name_if_changed(&mut w.last_uname, h.uid, &h.uname),
        gname: Writer::<O>::name_if_changed(&mut w.last_gname, h.gid, &h.gname),
        dev_major: if matches!(kind, "char" | "block") {
            h.devmajor
        } else {
            0
        },
        dev_minor: if matches!(kind, "char" | "block") {
            h.devminor
        } else {
            0
        },
        xattrs,
        ..Entry::default()
    })
}

/// `cleanEntryName`: the name as `path.Clean` of `/` and it makes it, without its root.
fn clean_name(name: &[u8]) -> Vec<u8> {
    let joined = shards_dockerfile::go::join(&[b"/", name]);
    joined.strip_prefix(b"/").map(<[u8]>::to_vec).unwrap_or(joined)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u8 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 56) as u8
        }
    }

    /// The oracle's `content`: text-like for the first half, random for the rest.
    fn content(seed: u64, n: usize) -> Vec<u8> {
        let text = b"the quick brown fox jumps over a lazy dog\n";
        let mut g = Lcg(seed);
        (0..n)
            .map(|i| {
                let b = g.next();
                if i < n / 2 { text[usize::from(b) % 42] } else { b }
            })
            .collect()
    }

    fn build(entries: Vec<(Header, Vec<u8>)>) -> Vec<u8> {
        let mut w = gotar::Writer::new(Vec::new());
        for (mut h, data) in entries {
            if h.typeflag == TYPE_REG {
                h.size = data.len() as i64;
            }
            w.write_header(&h).unwrap();
            w.write_all(&data).unwrap();
        }
        w.finish().unwrap()
    }

    /// The oracle's tars, built again.
    fn tars() -> BTreeMap<&'static str, Vec<u8>> {
        let t0 = gotar::Time::unix(1_600_000_000, 0);
        let tn = gotar::Time::unix(1_600_000_000, 600_000_000);
        let h = |typeflag: u8, name: &str, mode: i64| Header {
            typeflag,
            name: name.as_bytes().to_vec(),
            mode,
            mtime: t0,
            ..Header::default()
        };
        let named = |mut h: Header, uid: i64, uname: &str, gname: &str| {
            h.uid = uid;
            h.gid = uid;
            h.uname = uname.as_bytes().to_vec();
            h.gname = gname.as_bytes().to_vec();
            h
        };
        let mut link = h(TYPE_SYMLINK, "a/link", 0o777);
        link.linkname = b"small".to_vec();
        let mut hard = h(TYPE_LINK, "a/hard", 0);
        hard.linkname = b"a/small".to_vec();
        let mut x = named(h(TYPE_REG, "a/x", 0o644), 1000, "u", "g");
        x.mtime = tn;
        x.pax.insert(b"SCHILY.xattr.user.k".to_vec(), b"v".to_vec());
        x.pax
            .insert(b"SCHILY.xattr.security.capability".to_vec(), b"\x01\x02".to_vec());
        let mut null = h(TYPE_CHAR, "a/null", 0o666);
        null.devmajor = 1;
        null.devminor = 3;
        let mut uni = named(h(TYPE_REG, "a/ünïcode", 0o644), 1, "daemon", "");
        uni.gid = 0;
        uni.mtime = gotar::Time::unix(0, 0);
        let long = format!("a/{}", "long-name-".repeat(15));
        let mut out = BTreeMap::new();
        out.insert("empty", build(Vec::new()));
        out.insert(
            "basic",
            build(vec![
                (h(TYPE_DIR, "a/", 0o755), Vec::new()),
                (
                    named(h(TYPE_REG, "a/small", 0o644), 0, "root", "root"),
                    content(1, 100),
                ),
                (
                    named(h(TYPE_REG, "a/empty", 0o600), 0, "root", "root"),
                    Vec::new(),
                ),
                (link, Vec::new()),
                (hard, Vec::new()),
                (x, content(2, 5000)),
                (named(h(TYPE_REG, "a/y", 0o4755), 1000, "u", "g"), content(3, 513)),
                (h(TYPE_REG, &long, 0o644), content(4, 1)),
                (h(TYPE_FIFO, "a/fifo", 0o644), Vec::new()),
                (null, Vec::new()),
                (uni, content(5, 7)),
            ]),
        );
        out.insert(
            "big",
            build(vec![
                (h(TYPE_DIR, "big/", 0o755), Vec::new()),
                (h(TYPE_REG, "big/file", 0o644), content(6, (9 << 20) + 123)),
                (h(TYPE_REG, "big/after", 0o644), content(7, 4 << 20)),
            ]),
        );
        out
    }

    fn sha(b: &[u8]) -> String {
        format!("sha256:{}", hex(&Sha256::digest(b)))
    }

    /// Every case of testdata/estargz.json (`scripts/estargz/generate`): the tar built
    /// again byte for byte, then written at each level as stargz-snapshotter writes it.
    #[test]
    fn estargz_is_stargz_snapshotters() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("testdata/estargz.json")).unwrap();
        let tars = tars();
        assert_eq!(cases.len(), 12);
        for c in cases {
            let name = c["tar"].as_str().unwrap();
            let level = i32::try_from(c["level"].as_i64().unwrap()).unwrap();
            let tar = &tars[name];
            assert_eq!(sha(tar), c["tar_digest"], "the {name} tar");
            let (blob, written) = write(tar.as_slice(), Vec::new(), level).unwrap();
            let at = format!("{name} at level {level}");
            if let Some(h) = c["blob_hex"].as_str() {
                assert_eq!(hex(&blob), h, "{at}");
            }
            assert_eq!(blob.len() as u64, c["blob_len"].as_u64().unwrap(), "{at}");
            assert_eq!(sha(&blob), c["blob_digest"], "{at}");
            assert_eq!(written.toc_digest, c["toc_digest"], "{at}");
            assert_eq!(written.diff_id, c["diff_id"], "{at}");
            assert_eq!(written.uncompressed, c["uncompressed"].as_u64().unwrap(), "{at}");
        }
    }
}
