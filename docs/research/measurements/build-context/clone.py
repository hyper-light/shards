#!/usr/bin/env python3
"""What taking one file into a build's stage costs, by size: an APFS clone
(fclonefileat, then the unlink the stage's removal does) against reading it and
appending it to one pack file (no file made or removed per snapshot). Each sample
takes N files in a fresh directory; reports per-file n, p50, p90, p99 and max.

  clone.py DIR N RUNS
"""
import ctypes, os, statistics, sys, time
libc = ctypes.CDLL(None, use_errno=True)
libc.fclonefileat.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
AT_FDCWD = -2
d, n, runs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
def pct(xs, p):
    xs = sorted(xs); return xs[max(0, -(-len(xs) * p // 100) - 1)]
print(f"n={runs} samples of {n} files each; µs per file\n")
print("| size | method | p50 | p90 | p99 | max |\n|---|---|---|---|---|---|")
for size in [1 << 10, 16 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20]:
    src = os.path.join(d, f"src-{size}")
    os.makedirs(src, exist_ok=True)
    for i in range(n):
        p = os.path.join(src, str(i))
        if not os.path.exists(p):
            with open(p, "wb") as f: f.write(os.urandom(size))
    res = {"clone": [], "pack": []}
    for r in range(runs):
        for method in (["clone", "pack"] if r % 2 == 0 else ["pack", "clone"]):
            stage = os.path.join(d, f"stage-{method}")
            os.makedirs(stage, exist_ok=True)
            t = time.perf_counter()
            if method == "clone":
                for i in range(n):
                    fd = os.open(os.path.join(src, str(i)), os.O_RDONLY)
                    if libc.fclonefileat(fd, AT_FDCWD, os.path.join(stage, str(i)).encode(), 0) != 0:
                        sys.exit(f"clone: {os.strerror(ctypes.get_errno())}")
                    os.close(fd)
                for i in range(n):
                    os.unlink(os.path.join(stage, str(i)))
            else:
                with open(os.path.join(stage, "pack"), "wb") as pack:
                    for i in range(n):
                        fd = os.open(os.path.join(src, str(i)), os.O_RDONLY)
                        left = size
                        while left:
                            b = os.read(fd, min(left, 1 << 20)); pack.write(b); left -= len(b)
                        os.close(fd)
                os.unlink(os.path.join(stage, "pack"))
            res[method].append((time.perf_counter() - t) / n * 1e6)
    for m, xs in res.items():
        print(f"| {size >> 10} KiB | {m} | {pct(xs,50):.1f} | {pct(xs,90):.1f} | {pct(xs,99):.1f} | {max(xs):.1f} |")
