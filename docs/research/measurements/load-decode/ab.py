#!/usr/bin/env python3
"""Times `shards load -i ARCHIVE` for two builds, interleaved, each run in a fresh home
(PM M84). A_BIN_DIR and B_BIN_DIR each hold a release `shards` and `shardsd`.

    ab.py A_BIN_DIR B_BIN_DIR N ARCHIVE...

M84's archive: an image whose layers are not compressed, so the archive's own compression
is real work, then compressed whole:

    printf 'FROM golang:1.26.8\n' > Dockerfile
    docker buildx build --provenance=false \
      --output type=docker,dest=plain.tar,compression=uncompressed,force-compression=true .
    zstd -3 -T0 plain.tar -o plain.tar.zst; gzip -6 -c plain.tar > plain.tar.gz
"""
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def once(bindir, archive):
    home = tempfile.mkdtemp(prefix="load-decode-")
    env = dict(os.environ, SHARDS_HOME=home)
    try:
        start = time.perf_counter()
        r = subprocess.run([os.path.join(bindir, "shards"), "load", "-i", archive],
                           env=env, capture_output=True, text=True)
        took = time.perf_counter() - start
        if r.returncode != 0:
            raise SystemExit(f"{bindir} {archive}: {r.returncode}\n{r.stdout}\n{r.stderr}")
        return took
    finally:
        subprocess.run([os.path.join(bindir, "shards"), "daemon", "stop"], env=env,
                       capture_output=True)
        shutil.rmtree(home, ignore_errors=True)


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main():
    a, b, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
    for archive in sys.argv[4:]:
        times = {a: [], b: []}
        once(a, archive)  # warm the page cache
        for i in range(n):
            for d in ((a, b) if i % 2 == 0 else (b, a)):
                times[d].append(once(d, archive))
        for d, label in ((a, "A"), (b, "B")):
            xs = times[d]
            print(f"{os.path.basename(archive)} {label}: n={len(xs)} p50={pct(xs, .5):.3f}s "
                  f"p90={pct(xs, .9):.3f}s p99={pct(xs, .99):.3f}s max={max(xs):.3f}s "
                  f"mean={statistics.mean(xs):.3f}s")


main()
