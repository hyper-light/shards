#!/usr/bin/env python3
"""measure.py's output as the PM entry's tables: for each build, its rounds' builder at
its peak and at its end (its last line), and at each step's end: what is cold of its files
(inactive_file), its files, its anonymous memory and shmem, what it has plugged and what of
that lies past twice what it uses; and its VM process on the host at its peak and end.
Each as n, p50, p90, p99 and max over the rounds whose cache mounts were filled before
them (each run's rounds 2 on), with round 1, which fills them, beside: each run's alone, or
with --pool every run given as one (runs of one host).

    tables.py [--pool] OUT.txt...
"""
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


def read(path, run, into):
    """A run's header, and its guest, host and build lines into `into`, keyed by build and
    (run, round)."""
    guest, host, builds, head = into
    for line in open(path, errors="replace"):
        if line.startswith(("host ", "builds ")) and "=" not in line.split()[1]:
            head.append(line.rstrip())
        elif line.startswith("guest "):
            f = fields(line)
            guest[(f["build"], (run, int(f["round"])))].append(f)
        elif line.startswith("host t="):
            f = fields(line)
            host[(f["build"], (run, int(f["round"])))].append(f)
        elif line.startswith("build "):
            f = fields(line)
            builds[(f["build"], (run, int(f["round"])))] = f


def report(title, into):
    guest, host, builds, head = into
    print(f"== {title}")
    for h in head:
        print(h)
    for name in sorted({b for b, _ in builds}):
        # Its builds that succeeded alone: a failed one's lines end where it failed.
        rounds = sorted(r for b, r in builds if b == name and builds[(b, r)].get("exit") == "0")
        first = [r for r in rounds if r[1] == 1]
        print(f"-- {name}: rounds {[f'{run}.{r}' for run, r in rounds]}; "
              f"s {[builds[(name, r)]['s'] for r in rounds]}")
        for when in ("peak", "end"):
            for k in KEYS:
                per = {}
                for r in rounds:
                    vals = [int(f.get(k, 0)) for f in guest.get((name, r), [])]
                    if vals:
                        per[r] = max(vals) if when == "peak" else vals[-1]
                later = [v for r, v in per.items() if r[1] > 1]
                print(f"   {when:4} {k:15} round 1: {stats([per[r] for r in first if r in per])}; "
                      f"rounds 2+: {stats(later)}")
        # Each step's end, by its place among the build's step ends.
        ends = defaultdict(dict)
        for r in rounds:
            for i, f in enumerate(f for f in guest.get((name, r), []) if f.get("step_end") == "1"):
                ends[i][r] = f
        for i in sorted(ends):
            step = next(iter(ends[i].values())).get("step", "-")
            for k in ("inactive_file", "past_twice_used", "plugged"):
                later = [int(f.get(k, 0)) for r, f in ends[i].items() if r[1] > 1]
                once = [int(f.get(k, 0)) for r, f in ends[i].items() if r[1] == 1]
                print(f"   step end {i + 1} (#{step}) {k:15} round 1: {stats(once)}; rounds 2+: {stats(later)}")
        for k in ("footprint", "pss", "rss_file", "rss_anon"):
            per = {r: [int(f[k]) for f in host.get((name, r), []) if k in f] for r in rounds}
            if any(per.values()):
                peaks = [max(v) for r, v in per.items() if v and r[1] > 1]
                endv = [v[-1] for r, v in per.items() if v and r[1] > 1]
                once = [max(v) for r, v in per.items() if v and r[1] == 1]
                print(f"   host {k:11} peak: round 1: {stats(once)}; rounds 2+: {stats(peaks)}; end: {stats(endv)}")


def main(args):
    pool = "--pool" in args
    paths = [a for a in args if a != "--pool"]
    if pool:
        into = (defaultdict(list), defaultdict(list), {}, [])
        for run, path in enumerate(paths):
            read(path, run, into)
        report(" + ".join(paths), into)
        return
    for path in paths:
        into = (defaultdict(list), defaultdict(list), {}, [])
        read(path, 0, into)
        report(path, into)


if __name__ == "__main__":
    if len([a for a in sys.argv[1:] if a != "--pool"]) < 1:
        raise SystemExit(__doc__)
    main(sys.argv[1:])
