#!/usr/bin/env python3
"""What readahead on a mapping's faults gives a sequential read of a file its host does
not hold (D190: a guest's read of its writable layer is its host's faults on the layer's
mapping, MAP_SHARED, page by page): a file of SIZE MiB written and synced, its pages
dropped (POSIX_FADV_DONTNEED, until util-linux's fincore says none are resident), mapped
shared, advised MADV_NORMAL or MADV_RANDOM, and read a byte of each page in order. Each
round both, in an order that turns. Prints each advice's MiB/s: p50 and range of the
rounds, with host and OS.

    coldmap.py DIR [--size MIB] [--rounds N]

Linux only. DIR is on the file system to measure; its file is removed."""
import argparse, mmap, os, platform, statistics, subprocess, sys, time

if sys.platform != "linux":
    raise SystemExit("coldmap.py: Linux only, where POSIX_FADV_DONTNEED drops a file's pages")
p = argparse.ArgumentParser()
p.add_argument("dir")
p.add_argument("--size", type=int, default=1024)
p.add_argument("--rounds", type=int, default=5)
a = p.parse_args()

MIB, PAGE = 1 << 20, mmap.PAGESIZE
path = os.path.join(os.path.realpath(a.dir), "coldmap")


def resident():
    out = subprocess.run(["fincore", "--bytes", "--noheadings", "--output", "RES", path],
                         capture_output=True, text=True, check=True).stdout.strip()
    return int(out or 0)


def drop():
    for _ in range(50):
        fd = os.open(path, os.O_RDONLY)
        try:
            os.fsync(fd)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)
        if resident() == 0:
            return 0
        time.sleep(0.2)
    return resident()


def read(advice):
    fd = os.open(path, os.O_RDONLY)
    try:
        m = mmap.mmap(fd, a.size * MIB, mmap.MAP_SHARED, mmap.PROT_READ)
        m.madvise(advice)
        t = time.perf_counter()
        s = 0
        for at in range(0, a.size * MIB, PAGE):
            s += m[at]
        t = time.perf_counter() - t
        m.close()
        return a.size / t
    finally:
        os.close(fd)


with open(path, "wb") as f:
    for _ in range(a.size):
        f.write(os.urandom(MIB))
    f.flush()
    os.fsync(f.fileno())
got = {"normal": [], "random": []}
left = []
order = [("normal", mmap.MADV_NORMAL), ("random", mmap.MADV_RANDOM)]
try:
    for r in range(a.rounds):
        for name, advice in order[r % 2:] + order[:r % 2]:
            left.append(drop())
            got[name].append(read(advice))
finally:
    os.remove(path)
print(f"host: {platform.node()}, {platform.platform()}; {a.dir}; {a.size} MiB, {a.rounds} rounds; "
      f"resident before each read at most {max(left)} bytes")
for name, v in got.items():
    print(f"{name:7} MiB/s p50 {statistics.median(v):.1f} [{min(v):.1f}, {max(v):.1f}]")
