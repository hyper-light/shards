#!/usr/bin/env python3
"""measure.py's output as the PM entry's tables: for each size, phase and cache, each arm's
files (n), and of its rounds the median of their p50s, p90s and p99s with the p50s' and
p99s' ranges, and the largest max; then each arm's VM process memory, host mappings and
descriptors at a run's end (medians of the rounds), and the window arm's mappings and
removals; then the sets, each run's time and its window arm's figures.

    tables.py OUT
"""
import re
import statistics
import sys
from collections import defaultdict

ROUND = re.compile(r"^round (\d+) (fuse|window) (\S+) (cold|warm) load ([\d.]+) run ([\d.]+)s")
PHASE = re.compile(r"^  (first|again|overwrite|rewrite|stat)\s+n (\d+) p50 ([\d.]+) p90 ([\d.]+) p99 ([\d.]+) max ([\d.]+)")
SET = re.compile(r"^set (\w+) \((\d+) files\) round (\d+) (fuse|window) window (\S+) smallest (\S+) "
                 r"load ([\d.]+) run ([\d.]+)s statx-dax (\S+) of (\S+) first p50 ([\d.]+) p90 ([\d.]+) "
                 r"p99 ([\d.]+) max ([\d.]+); (.*)")
UNITS = {"k": 1 << 10, "m": 1 << 20}


def size_of(label):
    return int(label[:-1]) * UNITS[label[-1]] if label[-1] in UNITS else int(label)


def field(text, name):
    m = re.search(rf"\b{name} (\d+)", text)
    return int(m.group(1)) if m else None


def main(path):
    phases = defaultdict(list)
    ends = defaultdict(list)
    loads = []
    sets = defaultdict(list)
    cur = None
    for line in open(path, errors="replace"):
        m = ROUND.match(line)
        if m:
            cur = (m.group(3), m.group(4), m.group(2))
            loads.append(float(m.group(5)))
            continue
        m = PHASE.match(line)
        if m and cur:
            phases[(cur[0], m.group(1), cur[1], cur[2])].append(
                (int(m.group(2)),) + tuple(float(m.group(i)) for i in range(3, 7)))
            continue
        if cur and line.startswith("  setups"):
            ends[(cur[0], cur[2])].append(line)
            cur = None
            continue
        m = SET.match(line)
        if m:
            sets[(m.group(1), m.group(5), m.group(4), m.group(6))].append(
                {"files": int(m.group(2)), "run": float(m.group(8)), "dax": m.group(9),
                 "p50": float(m.group(11)), "p99": float(m.group(13)), "max": float(m.group(14)),
                 "rest": m.group(15), "load": float(m.group(7))})
    if loads:
        print(f"load at each run's start: median {statistics.median(loads):.1f}, range [{min(loads):.1f}, {max(loads):.1f}]")
    print("size phase cache arm: n; p50 median [range]; p90 median; p99 median [range]; max (µs)")
    for key in sorted(phases, key=lambda k: (size_of(k[0]), k[1], k[2], k[3])):
        rows = phases[key]
        n = sum(r[0] for r in rows)
        p50, p90, p99 = [r[1] for r in rows], [r[2] for r in rows], [r[3] for r in rows]
        print(f"{key[0]:>6} {key[1]:9} {key[2]} {key[3]:6}: n {n}; p50 {statistics.median(p50):.1f} "
              f"[{min(p50):.1f}, {max(p50):.1f}]; p90 {statistics.median(p90):.1f}; p99 "
              f"{statistics.median(p99):.1f} [{min(p99):.1f}, {max(p99):.1f}]; max {max(r[4] for r in rows):.1f}")
    print("size arm: VM memory KiB, host mappings, descriptors at a run's end (medians); window's maps, removes")
    for key in sorted(ends, key=lambda k: (size_of(k[0]), k[1])):
        lines = ends[key]
        def med(name):
            v = [field(l, name) for l in lines]
            v = [x for x in v if x is not None]
            return f"{statistics.median(v):.0f}" if v else "-"
        print(f"{key[0]:>6} {key[1]:6}: memory_kib {med('memory_kib')}, host_mappings {med('host_mappings')}, "
              f"descriptors {med('descriptors')}; maps {med('maps')}, removes {med('removes')}, peak {med('peak')}")
    if sets:
        print("set window arm smallest: runs; run s median [range]; first p50, p99, max (µs) medians; window's figures")
        for key in sorted(sets, key=lambda k: (k[0], k[1], k[2], int(k[3]) if k[3] != "-" else -1)):
            rows = sets[key]
            run = [r["run"] for r in rows]
            print(f"{key[0]:5} {key[1]:>7} {key[2]:6} {key[3]:>8}: {len(rows)}; {statistics.median(run):.2f} "
                  f"[{min(run):.2f}, {max(run):.2f}]; {statistics.median(r['p50'] for r in rows):.1f}, "
                  f"{statistics.median(r['p99'] for r in rows):.1f}, {max(r['max'] for r in rows):.1f}; "
                  f"dax {rows[0]['dax']} of {rows[0]['files']}; {rows[-1]['rest'][:300]}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
