#!/usr/bin/env python3
"""What writing back a restored layer's copy costs as it restores (D190, PM M278): the
blocks not all zeros of a template's kept layer, written into a fresh sparse file as a
restore's copy writes them (vmm `platform::copy_used`), then fsync(2)
(`platform::write_back_ahead`), ROUNDS times, each into a file of its own, removed after.

    copy-back.py LAYER DIR ROUNDS

LAYER is a template generation's `layer` file; DIR a directory on the disk the home is on.
Prints n, p50, p90, p99 and max of the copy and of the fsync, in milliseconds."""
import os, sys, time

src, dst_dir, rounds = sys.argv[1], sys.argv[2], int(sys.argv[3])
fd = os.open(src, os.O_RDONLY)
size = os.fstat(fd).st_size
# The ranges the file system says hold data, and of them the blocks not all zeros.
ranges, at = [], 0
while at < size:
    try:
        data = os.lseek(fd, at, os.SEEK_DATA)
    except OSError:
        break
    hole = os.lseek(fd, data, os.SEEK_HOLE)
    ranges.append((data, hole - data))
    at = hole
blocks = []
for start, n in ranges:
    for off in range(start, start + n, 4096):
        block = os.pread(fd, 4096, off)
        if any(block):
            blocks.append((off, block))
print(f"{len(ranges)} ranges, {len(blocks)} blocks not all zeros ({len(blocks) * 4} KiB)")
copied, synced = [], []
for r in range(rounds):
    path = os.path.join(dst_dir, f"copy-back-{os.getpid()}-{r}")
    out = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
    t = time.perf_counter()
    for off, block in blocks:
        os.pwrite(out, block, off)
    os.ftruncate(out, size)
    t1 = time.perf_counter()
    os.fsync(out)
    t2 = time.perf_counter()
    os.close(out)
    os.unlink(path)
    copied.append((t1 - t) * 1e3)
    synced.append((t2 - t1) * 1e3)


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


for name, xs in (("copy", copied), ("fsync", synced)):
    print(f"{name}: n {len(xs)} p50 {pct(xs, .5):.2f} p90 {pct(xs, .9):.2f} p99 {pct(xs, .99):.2f} "
          f"max {max(xs):.2f} ms")
