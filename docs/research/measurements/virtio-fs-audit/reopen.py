#!/usr/bin/env python3
"""What opening a directory again along its path costs (audit V09).

    reopen.py BINARY [--runs N] [--n SAMPLES] [--depths 1,4,16]

BINARY is this harness built against a revision with V09. For each depth, fresh
processes alternate between a GETATTR of a held directory and one let go between requests
and opened again along its path (`--case reopen [--let-go]`), and pool each arm's samples:
n, p50, p90, p99 and max in microseconds, and the paired difference of the processes'
medians (let go - held), the cost of a reopen, with a bootstrap 95% interval.
"""
import argparse, json, os, platform, random, subprocess

ap = argparse.ArgumentParser()
ap.add_argument("binary")
ap.add_argument("--runs", type=int, default=10)
ap.add_argument("--n", type=int, default=2000)
ap.add_argument("--depths", default="1,4,16")
args = ap.parse_args()


def q(v, p):
    v = sorted(v)
    return v[min(len(v), max(1, -(-len(v) * p // 100))) - 1]


def sample(depth, let_go):
    flags = ["--case", "reopen", "--n", str(args.n), "--depth", str(depth)] + (["--let-go"] if let_go else [])
    return [ns / 1000 for ns in json.loads(subprocess.check_output([args.binary, *flags], text=True))["ns"]]


print(f"host {platform.machine()} {platform.platform()} · load average {os.getloadavg()[0]:.1f}")
for depth in map(int, args.depths.split(",")):
    pooled = {False: [], True: []}
    medians = {False: [], True: []}
    for arm in (False, True):
        sample(depth, arm)
    for i in range(args.runs):
        for arm in ((False, True) if i % 2 == 0 else (True, False)):
            got = sample(depth, arm)
            pooled[arm] += got
            medians[arm].append(q(got, 50))
    for arm, name in ((False, "held"), (True, "let go")):
        v = pooled[arm]
        print(f"depth {depth:<3} {name:<7} n {len(v)} p50 {q(v, 50):.2f} p90 {q(v, 90):.2f}"
              f" p99 {q(v, 99):.2f} max {max(v):.2f} us")
    diffs = [g - h for g, h in zip(medians[True], medians[False])]
    boot = sorted(sorted(random.choices(diffs, k=len(diffs)))[len(diffs) // 2] for _ in range(2000))
    print(f"depth {depth:<3} a reopen, paired process medians: {sorted(diffs)[len(diffs) // 2]:.2f} us,"
          f" 95% [{boot[50]:.2f}, {boot[1949]:.2f}]")
