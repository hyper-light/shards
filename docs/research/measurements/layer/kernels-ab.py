#!/usr/bin/env python3
"""kernels-ab.sh's lines, by kernel and layer size: n, the guest's free memory once its
NVDIMM buses' probes are done (the `layer` verb's second free_kib), at p50 and max, and
what the layer takes of it against no layer by each (PM M277).

    kernels-ab.py FILE"""
import collections, statistics, sys

free = collections.defaultdict(list)
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) < 4 or not f[2].isdigit():
        continue
    probed = [int(x.split("=", 1)[1]) for x in f if x.startswith("free_kib=")]
    if len(probed) < 2:
        print("no probed memory:", line.strip())
        continue
    free[(f[0], int(f[2]))].append(probed[1] / 1024)
for kernel in ("old", "new"):
    holds = sorted(h for k, h in free if k == kernel)
    if 0 not in holds:
        continue
    p500, most0 = statistics.median(free[(kernel, 0)]), max(free[(kernel, 0)])
    for h in holds:
        v = free[(kernel, h)]
        print(f"{kernel} {h:3} GiB  n {len(v):2}  free p50 {statistics.median(v):7.1f} max {max(v):7.1f} MiB"
              f"  taken (p50) {p500 - statistics.median(v):6.1f} (max) {most0 - max(v):6.1f}")
