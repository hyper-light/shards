#!/usr/bin/env python3
"""mounts.sh's lines, by arm: n, and in µs at p50, p90, p99 and max the run's setup in its
microVM (its request to its command's start) and the whole run there (to its answer) (PM
M306).

    mounts.py FILE"""
import collections, sys

v = collections.defaultdict(lambda: ([], []))
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) < 4 or not f[1].isdigit() or not f[2].lstrip("-").isdigit():
        continue
    v[f[0]][0].append(int(f[2]))
    v[f[0]][1].append(int(f[3]))


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))]


def say(xs):
    return f"p50 {q(xs, .5)} p90 {q(xs, .9)} p99 {q(xs, .99)} max {max(xs)}"


for arm in v:
    setup, run = v[arm]
    print(f"{arm:14} n {len(setup)}  setup {say(setup)}  run {say(run)} µs")
