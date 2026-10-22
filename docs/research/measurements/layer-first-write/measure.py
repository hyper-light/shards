#!/usr/bin/env python3
"""What a run's first write into its writable layer costs its host (D190, PM M292: the
run's first data write in its root takes about 3 ms on Fedora, each run, a template's own
first write before its snapshot changing nothing): a store through a shared mapping of
the layer's file, as its guest's store through the pmem device reaches it, into a page
the file holds no data at, as ext4 writes a block it has just allocated.

Each round makes SOURCE, a sparse file of SIZE MiB holding a MiB of data at its start, as
a layer holds its file system's first blocks; then times, through a MAP_SHARED mapping of
each file, a store of a byte into three pages past its data, one after another: in
SOURCE itself, in a clone of it (FICLONE on Linux, clonefile(2) on macOS), as each warm
microVM's layer is a clone of its snapshot's (daemon/layers.rs), and in a clone whose first
page was written first with pwrite(2), with the bytes it holds (`touched`), as the daemon
could as it makes one, and in a clone mapped with MADV_RANDOM (`random`), which takes a
fault's readahead away. Each clone is made just before its stores, as a restore's is.
Prints each store's p50, p90 and max, µs, of the rounds, with host and OS.

    measure.py DIR [--rounds N] [--size MIB]

DIR is on the file system to measure (a shards home's); its files are removed."""
import argparse, ctypes, fcntl, mmap, os, platform, statistics, sys, time

p = argparse.ArgumentParser()
p.add_argument("dir")
p.add_argument("--rounds", type=int, default=20)
p.add_argument("--size", type=int, default=1024)
a = p.parse_args()

MIB = 1 << 20
PAGE = mmap.PAGESIZE
FICLONE = 0x40049409  # linux/fs.h _IOW(0x94, 9, int)


def clone(src, dst):
    if sys.platform == "darwin":
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.clonefile(src.encode(), dst.encode(), 0) != 0:
            raise OSError(ctypes.get_errno(), f"clonefile {dst}")
    else:
        with open(src, "rb") as s, open(dst, "wb") as d:
            fcntl.ioctl(d.fileno(), FICLONE, s.fileno())


def stores(path, random=False):
    """Three stores, each into its own page past the file's data, timed one by one."""
    fd = os.open(path, os.O_RDWR)
    try:
        m = mmap.mmap(fd, a.size * MIB, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
        if random:
            m.madvise(mmap.MADV_RANDOM)
        took = []
        for k in range(3):
            at = (a.size // 2) * MIB + k * 64 * PAGE
            t = time.perf_counter_ns()
            m[at] = 1
            took.append((time.perf_counter_ns() - t) / 1e3)
        m.close()
        return took
    finally:
        os.close(fd)


base = os.path.realpath(a.dir)
got = {"source": [[], [], []], "clone": [[], [], []], "touched": [[], [], []], "random": [[], [], []]}
for r in range(a.rounds):
    src = os.path.join(base, f"first-write-{r}")
    dst, touched, rand = src + ".clone", src + ".touched", src + ".random"
    with open(src, "wb") as f:
        f.write(os.urandom(MIB))
        f.truncate(a.size * MIB)
        f.flush()
        os.fsync(f.fileno())
    order = [("source", src), ("clone", dst), ("touched", touched), ("random", rand)]
    for which, path in order[r % 4:] + order[:r % 4]:
        if which != "source":
            clone(src, path)
        if which == "touched":
            fd = os.open(path, os.O_RDWR)
            try:
                os.pwrite(fd, os.pread(fd, PAGE, 0), 0)
            finally:
                os.close(fd)
        for i, us in enumerate(stores(path, random=which == "random")):
            got[which][i].append(us)
    for path in (dst, touched, rand, src):
        os.remove(path)

print(f"host: {platform.node()}, {platform.platform()}; {a.dir}; {a.rounds} rounds, {a.size} MiB files")
print(f"{'file':7} {'store':5} {'n':>4} {'p50':>9} {'p90':>9} {'max':>9}  us")
for which, per in got.items():
    for i, v in enumerate(per):
        s = sorted(v)
        p90 = s[min(len(s) - 1, int(0.9 * len(s)))]
        print(f"{which:7} {i + 1:5} {len(s):4} {statistics.median(s):9.1f} {p90:9.1f} {s[-1]:9.1f}")
