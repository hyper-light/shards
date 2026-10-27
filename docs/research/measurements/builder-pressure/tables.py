#!/usr/bin/env python3
"""measure.py's output as the PM entry's tables, each run alone or with --pool every run given
as one (runs of one host): for each build and arm (free, held), over the rounds after the
first (round 1's free arm fills the builds' caches): its time, its scope's anonymous peak and budget
(Linux), and its builder's give-backs, each answer's time from its word and what it freed
of its files and asked to unplug, and each build's sums; each as n, p50, p90, p99 and max.
Then the rule fixed before any number: a held build is slower than its free arm beyond the
rounds' spread where its fastest round is slower than the free arm's slowest.

    tables.py [--pool] OUT.txt...
"""
import sys
from collections import defaultdict

MIB = 1 << 20


def fields(line):
    return dict(w.split("=", 1) for w in line.split()[1:] if "=" in w)


def pct(values, q):
    v = sorted(values)
    return v[min(len(v) - 1, int(q / 100 * len(v)))]


def stats(values, unit="", scale=1):
    if not values:
        return "-"
    f = lambda v: f"{v / scale:.1f}" if scale != 1 or isinstance(v, float) else f"{v}"
    return (f"n {len(values)} p50 {f(pct(values, 50))} p90 {f(pct(values, 90))} p99 {f(pct(values, 99))} "
            f"max {f(max(values))}{unit}")


def read(path, run, into):
    builds, gives, head = into
    for line in open(path, errors="replace"):
        if line.startswith(("host ", "builds ")) and "=" not in line.split()[1]:
            head.append(line.rstrip())
        elif line.startswith("build "):
            f = fields(line)
            builds[(f["build"], f["arm"], (run, int(f["round"])))] = f
        elif line.startswith("give "):
            f = fields(line)
            gives[(f["build"], f["arm"], (run, int(f["round"])))].append(f)


def report(title, into):
    builds, gives, head = into
    print(f"== {title}")
    for h in head:
        print(h)
    for name in sorted({b for b, _, _ in builds}):
        times = {}
        for arm in ("free", "held"):
            rounds = sorted(r for b, m, r in builds if b == name and m == arm and r[1] > 1
                            and builds[(b, m, r)].get("exit") == "0")
            failed = sorted(r for b, m, r in builds if b == name and m == arm and builds[(b, m, r)].get("exit") != "0")
            s = [float(builds[(name, arm, r)]["s"]) for r in rounds]
            times[arm] = s
            print(f"-- {name} {arm}: rounds 2+ {len(rounds)}, failed {len(failed)}")
            print(f"   build time        {stats(s, ' s')}")
            peaks = [int(builds[(name, arm, r)].get("scope_anon_peak", 0)) for r in rounds]
            if any(peaks):
                print(f"   scope anon peak   {stats(peaks, ' MiB', MIB)}")
            budgets = [int(builds[(name, arm, r)].get("budget", 0)) for r in rounds]
            if any(budgets):
                print(f"   budget            {stats(budgets, ' MiB', MIB)}")
            answers = [g for r in rounds for g in gives.get((name, arm, r), []) if "answered_ms" in g]
            unanswered = [g for r in rounds for g in gives.get((name, arm, r), []) if "unanswered_ms" in g]
            if answers or unanswered:
                print(f"   answers           {len(answers)}, unanswered {len(unanswered)}")
                print(f"   answered in       {stats([int(g['answered_ms']) for g in answers], ' ms')}")
                print(f"   freed an answer   {stats([int(g['freed']) for g in answers], ' MiB', MIB)}")
                print(f"   unplugging        {stats([int(g['unplugging']) for g in answers], ' MiB', MIB)}")
                sums = [sum(int(g["freed"]) for g in gives.get((name, arm, r), []) if "answered_ms" in g) for r in rounds]
                print(f"   freed a build     {stats(sums, ' MiB', MIB)}")
        free, held = times.get("free", []), times.get("held", [])
        if free and held:
            worse = min(held) > max(free)
            print(f"   rule: held slower than free beyond the rounds' spread: {'yes' if worse else 'no'} "
                  f"(held's fastest {min(held):.1f} s, free's slowest {max(free):.1f} s)")


def main(args):
    pool = "--pool" in args
    paths = [a for a in args if a != "--pool"]
    if pool:
        into = ({}, defaultdict(list), [])
        for run, path in enumerate(paths):
            read(path, run, into)
        report(" + ".join(paths), into)
        return
    for path in paths:
        into = ({}, defaultdict(list), [])
        read(path, 0, into)
        report(path, into)


if __name__ == "__main__":
    if len([a for a in sys.argv[1:] if a != "--pool"]) < 1:
        raise SystemExit(__doc__)
    main(sys.argv[1:])
