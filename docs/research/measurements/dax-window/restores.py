#!/usr/bin/env python3
"""restores.sh's lines, by window length: the first run's time (ms), the template's size on
disk (MiB), the restored runs' times, paced and back to back (ms: n, p50, p90, max), and
the resident memory of the VM processes restored from the template, a detached run's and
its pool's (MiB: n, p50, max) (PM M306).

    restores.py FILE"""
import collections, statistics, sys

v = collections.defaultdict(lambda: collections.defaultdict(list))
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) == 3 and (f[0].isdigit() or f[0] == "default") and f[2].isdigit():
        v[f[0]][f[1]].append(int(f[2]))


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))]


# Lengths in MiB in order, then the daemon's default.
for length in sorted(v, key=lambda n: (not n.isdigit(), int(n) if n.isdigit() else 0)):
    m = v[length]
    rss = [k / 1024 for k in m["rss"]]
    named = f"{length:>6} MiB" if length.isdigit() else f"{length:>10}"
    line = f"{named}  first {m['first'] or '-'} ms  template {[round(k / 1024, 1) for k in m['template']] or '-'} MiB"
    for kind in ("paced", "burst"):
        runs = m[kind]
        if runs:
            line += f"  {kind} n {len(runs)} p50 {statistics.median(runs)} p90 {q(runs, .9)} max {max(runs)} ms"
    if rss:
        line += f"  rss n {len(rss)} p50 {statistics.median(rss):.1f} max {max(rss):.1f} MiB"
    print(line)
