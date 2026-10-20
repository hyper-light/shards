"""How a host's file system keeps a sparse file's holes between the blocks written to it
(D190, PM M277): a 1 GiB file made by truncate(2), two 4 KiB blocks written GAP blocks
apart, then synced; what it allocates and the data ranges lseek(2) reports. Then the
same blocks written far first, in descending order, synced after each, and through a
shared mapping, each in a fresh file.

usage: python3 -I sparse.py DIR    (prints one line a case)
"""

import mmap
import os
import sys

BLOCK = 4096


def ranges(fd):
    """(allocated KiB, the data ranges in blocks) of the file at `fd`."""
    st = os.fstat(fd)
    found, at = [], 0
    while at < st.st_size:
        try:
            start = os.lseek(fd, at, os.SEEK_DATA)
        except OSError:
            break
        end = os.lseek(fd, start, os.SEEK_HOLE)
        found.append((start // BLOCK, end // BLOCK))
        at = end
    return st.st_blocks * 512 // 1024, found


def case(directory, name, blocks, sync_each=False, mapped=False):
    path = os.path.join(directory, "sparse-" + name)
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.ftruncate(fd, 1 << 30)
        if mapped:
            m = mmap.mmap(fd, 1 << 30, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
            for b in blocks:
                m[b * BLOCK:(b + 1) * BLOCK] = b"z" * BLOCK
            m.flush()
            m.close()
        else:
            for b in blocks:
                os.pwrite(fd, b"z" * BLOCK, b * BLOCK)
                if sync_each:
                    os.fsync(fd)
        os.fsync(fd)
        kib, found = ranges(fd)
        print("%-24s blocks %-24s allocated %6d KiB  data %s" % (name, blocks, kib, found))
    finally:
        os.close(fd)
        os.unlink(path)


def main():
    directory = sys.argv[1]
    for gap in (2, 16, 256, 1024, 4096, 8192):
        case(directory, "gap-%d" % gap, [0, gap])
    case(directory, "far-then-near", [0, 16384, 256])
    case(directory, "descending", [4096, 2048, 1024, 0])
    case(directory, "synced-each", [0, 1024, 2048], sync_each=True)
    case(directory, "inside-a-hole", [100000, 102400])
    case(directory, "mapped", [0, 1024, 2048, 100000], mapped=True)


main()
