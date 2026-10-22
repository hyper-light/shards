#!/usr/bin/env python3
"""What a server's pwrite(2) of 4 KiB costs its host where the file's pages are mapped by
another mapping too, as a DAX window's server writes a FUSE_WRITE into a file the VM
process maps for the window (D189, PM M299): a file of SIZE MiB, laid out with random
bytes and synced, written N times at random page-aligned places by pwrite, each timed:
`alone`, nothing else mapping it; `mapped`, the whole file mapped shared and read-only
first, each 2 MiB range's first page read through it, as a read maps a window's range. The
same places each. Each round both, in an order that turns. Prints each's p50, p90 and max,
µs, of the rounds.

    mapped_pwrite.py DIR [--size MIB] [--writes N] [--rounds N]

DIR is on the file system to measure; its file is removed."""
import argparse, mmap, os, platform, random, statistics, time

p = argparse.ArgumentParser()
p.add_argument("dir")
p.add_argument("--size", type=int, default=2048)
p.add_argument("--writes", type=int, default=5000)
p.add_argument("--rounds", type=int, default=5)
a = p.parse_args()

MIB, PAGE, RANGE = 1 << 20, 4096, 2 << 20
path = os.path.join(os.path.realpath(a.dir), "mapped-pwrite")
with open(path, "wb") as f:
    for _ in range(a.size):
        f.write(os.urandom(MIB))
    f.flush()
    os.fsync(f.fileno())
places = random.Random(9).sample(range(a.size * MIB // PAGE), a.writes)
page = os.urandom(PAGE)


def run(mapped):
    fd = os.open(path, os.O_RDWR)
    m = None
    try:
        if mapped:
            m = mmap.mmap(fd, a.size * MIB, mmap.MAP_SHARED, mmap.PROT_READ)
            for at in range(0, a.size * MIB, RANGE):
                _ = m[at]
        took = []
        for n in places:
            t = time.perf_counter_ns()
            os.pwrite(fd, page, n * PAGE)
            took.append((time.perf_counter_ns() - t) / 1e3)
        return took
    finally:
        if m is not None:
            m.close()
        os.close(fd)


got = {"alone": [], "mapped": []}
try:
    for r in range(a.rounds):
        for kind in (["alone", "mapped"] if r % 2 == 0 else ["mapped", "alone"]):
            took = sorted(run(kind == "mapped"))
            got[kind].append((statistics.median(took), took[int(0.9 * len(took))], took[-1]))
finally:
    os.remove(path)
print(f"host: {platform.node()}, {platform.platform()}; {a.dir}; {a.size} MiB, {a.writes} writes, {a.rounds} rounds")
for kind, rows in got.items():
    p50s, p90s, maxs = zip(*rows)
    print(f"{kind:7} p50 {statistics.median(p50s):.1f} [{min(p50s):.1f}, {max(p50s):.1f}]  "
          f"p90 {statistics.median(p90s):.1f}  max {max(maxs):.1f}  us")
