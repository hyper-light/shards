#!/usr/bin/env python3
"""heads-ab.sh's lines, by kernel and window length: n, free memory at PID 1 (MiB) at p50 and
the most of the boots, what the window takes against no window by each, and PID 1's uptime
(s, 10 ms steps) at p50 and max (PM M300). A boot's free memory may fall in a lower band of
its own, the same for every length; the most free of a length's boots is what its window
leaves, against the most free with none.

    heads-ab.py FILE"""
import collections, statistics, sys

free = collections.defaultdict(list)
up = collections.defaultdict(list)
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) < 8 or not f[2].isdigit():
        continue
    kernel, length = f[0], int(f[2])
    free[(kernel, length)].append(int(f[5]) / 2**20)
    up[(kernel, length)].append(float(f[7]))
for kernel in ("old", "new"):
    lengths = sorted(l for k, l in free if k == kernel)
    if 0 not in lengths:
        continue
    most0, p500 = max(free[(kernel, 0)]), statistics.median(free[(kernel, 0)])
    for length in lengths:
        v, u = free[(kernel, length)], up[(kernel, length)]
        print(f"{kernel} {length:5} MiB  n {len(v):2}  free p50 {statistics.median(v):6.1f} most {max(v):6.1f}"
              f"  taken (most) {most0 - max(v):5.1f} (p50) {p500 - statistics.median(v):5.1f}"
              f"  PID 1 at p50 {statistics.median(u):.2f} max {max(u):.2f}")
