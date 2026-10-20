"""Records what scripts/ext4/generate's container found: e2fsck's word on each image, with
the image's digest as shards-ext4's tests compute it (crates/ext4/src/tests.rs,
Image::digest), and the structures of mke2fs's file system.

usage: record.py OUT DATA
"""

import hashlib
import json
import os
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


main()
