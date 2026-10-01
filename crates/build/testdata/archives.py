#!/usr/bin/env python3
"""Writes archives.json, the archives the ADD cases of ops.json and tests/build.rs in
crates/shards unpack, deterministically: fixed times,
owners and order, gzip without a name or time, and zstd from the `zstd` tool. Prints
them as JSON, name to base64.
"""
import base64, bz2, gzip, io, json, lzma, subprocess, sys, tarfile

T = 1600000000

def tar(entries, fmt=tarfile.PAX_FORMAT):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=fmt) as t:
        for e in entries:
            i = tarfile.TarInfo(e["name"])
            i.mtime = e.get("mtime", T)
            i.mode = e.get("mode", 0o644)
            i.uid, i.gid = e.get("uid", 0), e.get("gid", 0)
            i.type = {"file": tarfile.REGTYPE, "dir": tarfile.DIRTYPE, "symlink": tarfile.SYMTYPE,
                      "hardlink": tarfile.LNKTYPE, "fifo": tarfile.FIFOTYPE, "char": tarfile.CHRTYPE}[e.get("type", "file")]
            i.linkname = e.get("link", "")
            i.devmajor, i.devminor = e.get("dev", (0, 0))
            if "xattrs" in e:
                i.pax_headers = {"SCHILY.xattr." + k: v for k, v in e["xattrs"].items()}
            data = e.get("data", "").encode()
            i.size = len(data) if i.type == tarfile.REGTYPE else 0
            t.addfile(i, io.BytesIO(data) if i.type == tarfile.REGTYPE else None)
    return out.getvalue()

simple = tar([
    {"name": "a/", "type": "dir", "mode": 0o750},
    {"name": "a/x", "data": "x-data", "mode": 0o640, "uid": 1000, "gid": 1000},
    {"name": "a/l", "type": "symlink", "link": "x"},
    {"name": "a/h", "type": "hardlink", "link": "a/x"},
    {"name": "a/s", "data": "suid", "mode": 0o4755},
    {"name": "a/k", "data": "k", "xattrs": {"user.k": "v"}},
    {"name": "a/p", "type": "fifo", "mode": 0o600},
    {"name": "a/null", "type": "char", "dev": (1, 3), "mode": 0o666},
    {"name": "b", "data": "top", "mtime": T + 7},
])
def gz(b):
    o = io.BytesIO()
    with gzip.GzipFile(fileobj=o, mode="wb", mtime=0, filename="") as f: f.write(b)
    return o.getvalue()
zst = subprocess.run(["zstd", "-q", "-c", "--no-check"], input=simple, capture_output=True, check=True).stdout
evil = tar([
    {"name": "../escape", "data": "up"},
    {"name": "/abs/file", "data": "abs"},
    {"name": "out", "type": "symlink", "link": "/etc"},
    {"name": "out/passwd", "data": "written through"},
    {"name": "up", "type": "symlink", "link": "../../.."},
    {"name": "up/far", "data": "far"},
    {"name": "hl", "type": "hardlink", "link": "../../etc/passwd"},
    {"name": "loop", "type": "symlink", "link": "loop"},
], fmt=tarfile.GNU_FORMAT)
implied = tar([{"name": "deep/no/parents/f", "data": "f"}])
replace = tar([
    {"name": "file-to-dir/", "type": "dir"},
    {"name": "dir-to-file", "data": "now a file"},
    {"name": "merge/", "type": "dir", "mode": 0o700},
    {"name": "merge/new", "data": "n"},
])
dot = tar([{"name": "./", "type": "dir", "mode": 0o700}, {"name": "./in-dot", "data": "d"}])
times = tar([{"name": "old", "data": "o", "mtime": -5}, {"name": "far", "data": "f", "mtime": 99999999999}])
print(json.dumps({k: base64.b64encode(v).decode() for k, v in {
    "simple.tar": simple, "simple.tar.gz": gz(simple), "simple.tar.bz2": bz2.compress(simple),
    "simple.tar.xz": lzma.compress(simple, format=lzma.FORMAT_XZ), "simple.tar.zst": zst,
    "evil.tar": evil, "implied.tar": implied, "replace.tar": replace, "dot.tar": dot, "times.tar": times,
    "plain.gz": gz(b"just text\n"), "fake.tar": b"not an archive at all\n" * 40,
}.items()}))
