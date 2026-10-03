#!/usr/bin/env python3
"""Interleaved A/B of `build-memory` (two builds of it, against two revisions of
shards-image): the time and allocations of every layer apply, summed, and the time of
writing the image, for three workloads (PM M85, M86):

- MANY.tar alone, every directory the layer's own (M78's archive, uncompressed:
  `d{n/100}/f{n}` for a million n, USTAR);
- MANY.tar over itself, every directory a lower layer's;
- LAYER.tar..., an image's uncompressed layers in order, the last applied over the rest.

    apply-ab.py OLD_BIN NEW_BIN N MANY.tar LAYER.tar...
"""
import re
import statistics
import subprocess
import sys

line = re.compile(r"^(below|apply)\s+([\d.]+) ms .* allocations (\d+) ")
write = re.compile(r"^write\s+([\d.]+) ms ")


def once(binary, args):
    out = subprocess.run([binary] + args, capture_output=True, text=True, check=True).stdout
    ms = allocs = written = 0.0
    for l in out.splitlines():
        m = line.match(l)
        if m:
            ms += float(m.group(2))
            allocs += int(m.group(3))
        m = write.match(l)
        if m:
            written = float(m.group(1))
    return ms, allocs, written


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


old, new, n, many, layers = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5:]
workloads = {
    "many": [many],
    "many over many": ["--below", many, many],
    "image": [a for l in layers[:-1] for a in ("--below", l)] + layers[-1:],
}
for name, args in workloads.items():
    if name == "image" and not layers:
        continue
    got = {old: [], new: []}
    once(old, args)
    once(new, args)
    for i in range(n):
        for b in ((old, new) if i % 2 == 0 else (new, old)):
            got[b].append(once(b, args))
    for b, label in ((old, "old"), (new, "new")):
        ms = [g[0] for g in got[b]]
        allocs = sorted({g[1] for g in got[b]})
        wr = [g[2] for g in got[b]]
        print(f"{name} {label}: n={len(ms)} apply ms p50={pct(ms, .5):.1f} p90={pct(ms, .9):.1f} "
              f"max={max(ms):.1f} mean={statistics.mean(ms):.1f}  allocations {allocs}  "
              f"write ms p50={pct(wr, .5):.1f} p90={pct(wr, .9):.1f} max={max(wr):.1f}")
