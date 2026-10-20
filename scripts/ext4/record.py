"""Records what scripts/ext4/generate's container found: e2fsck's word on each image, with
the image's digest as shards-ext4's tests compute it (crates/ext4/src/tests.rs,
Image::digest), the structures of mke2fs's file system, and the tree tree.sh made and what
debugfs read of it (tree.blocks.gz, tree.json).

usage: record.py OUT DATA
"""

import gzip
import hashlib
import json
import os
import re
import struct
import sys

BLOCK = 4096


def data_blocks(path):
    """(number, bytes) of each block not all zeros, read through the file's data alone."""
    fd = os.open(path, os.O_RDONLY)
    try:
        end = os.fstat(fd).st_size
        at = 0
        while at < end:
            try:
                start = os.lseek(fd, at, os.SEEK_DATA)
            except OSError:
                break
            stop = os.lseek(fd, start, os.SEEK_HOLE)
            start -= start % BLOCK
            for off in range(start, stop, BLOCK):
                b = os.pread(fd, BLOCK, off).ljust(BLOCK, b"\0")
                if any(b):
                    yield off // BLOCK, b
            at = stop
    finally:
        os.close(fd)


def digest(path):
    h = hashlib.sha256()
    for n, b in data_blocks(path):
        h.update(struct.pack("<Q", n))
        h.update(b)
    return h.hexdigest()


def main():
    out, data = sys.argv[1], sys.argv[2]
    cases = []
    for name in sorted(os.listdir(out), key=lambda n: (len(n), n)):
        if not name.endswith(".img"):
            continue
        path = os.path.join(out, name)
        with open(path + ".exit") as f:
            code = int(f.read().strip())
        with open(path + ".out") as f:
            output = f.read()
        cases.append({"bytes": int(name[:-4]), "digest": digest(path), "exit": code, "output": output})
    with open(os.path.join(data, "e2fsck.json"), "w") as f:
        json.dump(cases, f, indent=1)
        f.write("\n")

    raw = os.path.join(out, "mke2fs.raw")
    with open(raw, "rb") as f:
        f.seek(1024)
        sb = f.read(1024)
        blocks = struct.unpack_from("<I", sb, 0x04)[0]
        per_group = struct.unpack_from("<I", sb, 0x20)[0]
        groups = -(-blocks // per_group)
        f.seek(BLOCK)
        descs = f.read(groups * 64)
        table = struct.unpack_from("<I", descs, 0x08)[0]
        f.seek(table * BLOCK)
        inodes = f.read(11 * 256)
        root = inodes[256:512]
        # The root's one extent: its start, high then low.
        hi, lo = struct.unpack_from("<HI", root, 0x28 + 12 + 6)
        root_block = (hi << 32) | lo
        f.seek(root_block * BLOCK)
        root_dir = f.read(BLOCK)
    with open(os.path.join(data, "mke2fs.json"), "w") as f:
        json.dump(
            {
                "superblock": sb.hex(),
                "descriptors": descs.hex(),
                "inodes": inodes.hex(),
                "root": root_dir.hex(),
                "root_block": root_block,
            },
            f,
            indent=1,
        )
        f.write("\n")
    record_tree(out, data)


def commands(path):
    """debugfs -f's output: each command it echoed (`debugfs: CMD`), with what followed."""
    parts, cmd, lines = [], None, []
    with open(path, encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("debugfs: "):
                if cmd is not None:
                    parts.append((cmd, lines))
                cmd, lines = line[len("debugfs: "):], []
            elif cmd is not None:
                lines.append(line)
    if cmd is not None:
        parts.append((cmd, lines))
    return parts


def time_of(lines, name):
    """An inode time debugfs prints raw (`name: 0xSECONDS:EXTRA`), decoded as the kernel
    decodes it (ext4_decode_extra_time): the low 32 bits signed, the extra's two epoch bits
    above them, its other 30 the nanoseconds."""
    for line in lines:
        m = re.match(r"\s*%s: 0x([0-9a-f]+):([0-9a-f]+)" % name, line)
        if m:
            lo, extra = int(m.group(1), 16), int(m.group(2), 16)
            sec = lo - (1 << 32) if lo & (1 << 31) else lo
            return [sec + ((extra & 3) << 32), extra >> 2]
    raise SystemExit("no %s in %r" % (name, lines))


def kernel_acl(value):
    """An ACL as the kernel gives it (posix_acl_to_xattr): ACL_UNDEFINED_ID for each entry
    of no user or group, where debugfs gives 0 (lib/ext2fs/ext_attr.c,
    convert_disk_buffer_to_posix_acl)."""
    out = bytearray(value)
    for at in range(4, len(out), 8):
        tag = struct.unpack_from("<H", out, at)[0]
        if tag not in (0x02, 0x08):
            struct.pack_into("<I", out, at + 4, 0xFFFFFFFF)
    return bytes(out)


TYPES = {
    "regular": "file",
    "directory": "dir",
    "symlink": "symlink",
    "character special": "char",
    "block special": "block",
    "FIFO": "fifo",
    "socket": "socket",
}


def record_tree(out, data):
    with open(os.path.join(out, "e2fsck.exit")) as f:
        if f.read().strip() != "0":
            with open(os.path.join(out, "e2fsck.out")) as g:
                raise SystemExit("e2fsck found the tree's file system damaged:\n" + g.read())
    with open(os.path.join(out, "paths"), encoding="utf-8", errors="surrogateescape") as f:
        paths = [line.rstrip("\n") for line in f]
    index = {p: n for n, p in enumerate(paths)}
    entries = {}
    for cmd, lines in commands(os.path.join(out, "stat.out")):
        if not cmd.startswith("stat "):
            continue
        path = cmd[len('stat "'):-1]
        head = re.match(r"Inode: (\d+)\s+Type: (.+?)\s+Mode:\s+(\d+)\s+Flags: 0x([0-9a-f]+)", lines[0])
        if not head:
            raise SystemExit("stat %s: %r" % (path, lines[:3]))
        owner = next(re.search(r"User:\s+(\d+)\s+Group:\s+(\d+)\s.*Size: (\d+)", l) for l in lines if l.startswith("User:"))
        links = next(re.search(r"Links: (\d+)", l) for l in lines if l.startswith("Links:"))
        e = {
            "path": path,
            "ino": int(head.group(1)),
            "type": TYPES[head.group(2)],
            "mode": int(head.group(3), 8),
            "flags": int(head.group(4), 16),
            "uid": int(owner.group(1)),
            "gid": int(owner.group(2)),
            "size": int(owner.group(3)),
            "links": int(links.group(1)),
            "mtime": time_of(lines, "mtime"),
            "atime": time_of(lines, "atime"),
            "ctime": time_of(lines, "ctime"),
        }
        for l in lines:
            m = re.match(r'Fast link dest: "(.*)"$', l)
            if m:
                e["target"] = m.group(1)
            m = re.match(r"(?:\(New-style\) )?Device major/minor number: (\d+):(\d+) ", l)
            if m:
                e["device"] = [int(m.group(1)), int(m.group(2))]
        dump = os.path.join(out, "dump", str(index[path]))
        if e["type"] == "file":
            with open(dump, "rb") as f:
                e["sha256"] = hashlib.sha256(f.read()).hexdigest()
        if e["type"] == "symlink" and "target" not in e:
            with open(dump, "rb") as f:
                e["target"] = f.read().decode("utf-8", "surrogateescape")
        e["xattrs"] = {}
        entries[path] = e
    for cmd, lines in commands(os.path.join(out, "ea.out")):
        path = cmd[len('ea_list "'):-1]
        for l in lines:
            m = re.match(r"\s+(\S+) \((\d+)\)", l)
            if not m:
                continue
            name, size = m.group(1), int(m.group(2))
            with open(os.path.join(out, "ea", "%d.%s" % (index[path], name)), "rb") as f:
                value = f.read()
            if name.startswith("system.posix_acl") is False and len(value) != size:
                raise SystemExit("%s %s: %d bytes, ea_list said %d" % (path, name, len(value), size))
            if name.startswith("system.posix_acl"):
                value = kernel_acl(value)
            entries[path]["xattrs"][name] = value.hex()
    for p in paths:
        if p not in entries:
            raise SystemExit("debugfs said nothing of " + p)
    # Each directory's names, as find walked them.
    children = {}
    for p in paths:
        parent, name = p.rsplit("/", 1)
        children.setdefault(parent or "/", []).append(name)
    tree = [{"path": "/", "children": sorted(children.get("/", []))}]
    for p in paths:
        e = entries[p]
        if e["type"] == "dir":
            e["children"] = sorted(children.get(p, []))
        tree.append(e)
    with open(os.path.join(data, "tree.json"), "w") as f:
        json.dump(tree, f, indent=1, ensure_ascii=False)
        f.write("\n")
    # The image: each block not all zeros, its number (32 bits) then its bytes.
    for name in ("tree", "journal"):
        raw = os.path.join(out, name + ".raw")
        with gzip.GzipFile(os.path.join(data, name + ".blocks.gz"), "wb", mtime=0) as f:
            f.write(struct.pack("<Q", os.path.getsize(raw)))
            for n, b in data_blocks(raw):
                f.write(struct.pack("<I", n))
                f.write(b)
    # What debugfs made of the journal's image, replayed: every block not all zeros, by
    # its SHA-256, and what its log held, as logdump read it.
    with open(os.path.join(out, "replayed-e2fsck.exit")) as f:
        if f.read().strip() != "0":
            with open(os.path.join(out, "replayed-e2fsck.out")) as g:
                raise SystemExit("e2fsck found the replayed file system damaged:\n" + g.read())
    with open(os.path.join(out, "logdump.out")) as f:
        logdump = f.read()
    with open(os.path.join(out, "multi.blocks")) as f:
        multi = [int(b) for b in f.read().split()]
    replayed = {
        "multi": multi,
        "logdump": logdump,
        "blocks": {str(n): hashlib.sha256(b).hexdigest() for n, b in data_blocks(os.path.join(out, "replayed.raw"))},
    }
    with open(os.path.join(data, "replayed.json"), "w") as f:
        json.dump(replayed, f, indent=1)
        f.write("\n")


main()
