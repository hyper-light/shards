#!/usr/bin/env python3
"""costs.sh's lines, by window length: n; PID 1's uptime (s, 10 ms steps) at p50 and max;
what the window takes of the guest's free memory against none (MiB, at p50 and by the most
free), free memory being MemFree and the pages its CPUs' lists hold; and the VM process's
peak resident memory (MiB) and minor faults at p50 and max (PM M306).

    costs.py FILE"""
import collections, statistics, sys

rows = collections.defaultdict(list)
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) < 9 or not f[0].isdigit() or f[2] == "none":
        continue
    length = int(f[0])
    free = (int(f[3]) + int(f[6])) / 2**20
    rss = int(f[7]) / 2**20 if f[7].isdigit() else float("nan")
    minor = int(f[8]) if f[8].isdigit() else -1
    rows[length].append((float(f[5]), free, rss, minor))
if 0 not in rows:
    sys.exit("no boots without a window to set beside the others")
free0 = [r[1] for r in rows[0]]
p500, most0 = statistics.median(free0), max(free0)
for length in sorted(rows):
    r = rows[length]
    up = [x[0] for x in r]
    free = [x[1] for x in r]
    rss = [x[2] for x in r]
    minor = [x[3] for x in r]
    print(f"{length:6} MiB  n {len(r):2}  PID 1 p50 {statistics.median(up):.2f} max {max(up):.2f} s"
          f"  taken p50 {p500 - statistics.median(free):6.1f} (most) {most0 - max(free):6.1f} MiB"
          f"  rss p50 {statistics.median(rss):7.1f} max {max(rss):7.1f} MiB"
          f"  faults p50 {statistics.median(minor):8.0f} max {max(minor):8d}")
