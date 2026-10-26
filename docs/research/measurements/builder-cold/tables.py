#!/usr/bin/env python3
"""measure.py's output as the PM entry's tables: for each build, its rounds' builder at
its peak and at its end (its last line), and at each step's end: what is cold of its files
(inactive_file), its files, its anonymous memory and shmem, what it has plugged and what of
that lies past twice what it uses; and its VM process on the host at its peak and end.
Each as n, p50, p90, p99 and max over the rounds whose cache mounts were filled before
them (rounds 2 on), with round 1, which fills them, beside.

    tables.py OUT.txt...
"""
import statistics
import sys
from collections import defaultdict

KEYS = ("inactive_file", "file", "anon", "shmem", "plugged", "past_twice_used", "current")
MIB = 1 << 20


def fields(line):
    return dict(w.split("=", 1) for w in line.split()[1:] if "=" in w)


def pct(values, q):
    v = sorted(values)
    return v[min(len(v) - 1, int(q / 100 * len(v)))]


def stats(values):
    if not values:
        return "-"
    return (f"n {len(values)} p50 {pct(values, 50) / MIB:.0f} p90 {pct(values, 90) / MIB:.0f} "
            f"p99 {pct(values, 99) / MIB:.0f} max {max(values) / MIB:.0f} MiB")


def main(paths):
    for path in paths:
        guest, host, builds = defaultdict(list), defaultdict(list), {}
        head = []
        for line in open(path, errors="replace"):
            if line.startswith(("host ", "builds ")) and "=" not in line.split()[1]:
                head.append(line.rstrip())
            elif line.startswith("guest "):
                f = fields(line)
                guest[(f["build"], int(f["round"]))].append(f)
            elif line.startswith("host t="):
                f = fields(line)
                host[(f["build"], int(f["round"]))].append(f)
            elif line.startswith("build "):
                f = fields(line)
                builds[(f["build"], int(f["round"]))] = f
        print(f"== {path}")
        for h in head:
            print(h)
        for name in sorted({b for b, _ in builds}):
            rounds = sorted(r for b, r in builds if b == name)
            print(f"-- {name}: rounds {rounds}; exits {[builds[(name, r)]['exit'] for r in rounds]}; "
                  f"s {[builds[(name, r)]['s'] for r in rounds]}")
            for when in ("peak", "end"):
                for k in KEYS:
                    per = {}
                    for r in rounds:
                        lines = guest.get((name, r), [])
                        vals = [int(f.get(k, 0)) for f in lines]
                        if vals:
                            per[r] = max(vals) if when == "peak" else vals[-1]
                    later = [v for r, v in per.items() if r > 1]
                    print(f"   {when:4} {k:15} round 1 {per.get(1, 0) / MIB:.0f} MiB; rounds 2+: {stats(later)}")
            # Each step's end, by its place among the build's step ends.
            ends = defaultdict(lambda: defaultdict(dict))
            for r in rounds:
                for i, f in enumerate(f for f in guest.get((name, r), []) if f.get("step_end") == "1"):
                    ends[i][r] = f
            for i in sorted(ends):
                for k in ("inactive_file", "past_twice_used", "plugged"):
                    later = [int(f.get(k, 0)) for r, f in ends[i].items() if r > 1]
                    first = int(ends[i].get(1, {}).get(k, 0))
                    print(f"   step end {i + 1} (#{next(iter(ends[i].values())).get('step', '-')}) {k:15} "
                          f"round 1 {first / MIB:.0f} MiB; rounds 2+: {stats(later)}")
            for k in ("footprint", "pss", "rss_file", "rss_anon"):
                per = {r: [int(f[k]) for f in host.get((name, r), []) if k in f] for r in rounds}
                if any(per.values()):
                    peaks = [max(v) for r, v in per.items() if v and r > 1]
                    endv = [v[-1] for r, v in per.items() if v and r > 1]
                    print(f"   host {k:11} peak: round 1 {max(per.get(1) or [0]) / MIB:.0f} MiB; rounds 2+: "
                          f"{stats(peaks)}; end: {stats(endv)}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    main(sys.argv[1:])
