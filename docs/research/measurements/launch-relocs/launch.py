#!/usr/bin/env python3
"""What a program's launch costs: each COMMAND spawned and reaped N times, the commands
in turn (A B C, A B C, ...), so that a host's load falls on each alike. For each: the
spawn to the reap, in ms, and the minor and major page faults its process took (the
children's rusage before and after), as n, p50, p90, p99 and max, with host, OS, CPUs and
load. A command is a shell-split string: `launch.py 500 /usr/bin/true "shards --version"`.

    launch.py N [--warmup W] COMMAND...
"""
import argparse, os, platform, resource, shlex, subprocess, time

p = argparse.ArgumentParser()
p.add_argument("n", type=int)
p.add_argument("--warmup", type=int, default=20)
p.add_argument("commands", nargs="+")
a = p.parse_args()
cmds = [shlex.split(c) for c in a.commands]


def one(argv):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.perf_counter_ns()
    subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t1 = time.perf_counter_ns()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    return (t1 - t0) / 1e6, after.ru_minflt - before.ru_minflt, after.ru_majflt - before.ru_majflt


for _ in range(a.warmup):
    for c in cmds:
        one(c)
rows = {i: [] for i in range(len(cmds))}
load = [os.getloadavg()[0]]
for k in range(a.n):
    for i, c in enumerate(cmds):
        rows[i].append(one(c))
    if k % 100 == 0:
        load.append(os.getloadavg()[0])
load.append(os.getloadavg()[0])


def pct(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, max(0, round(q / 100 * len(v) + 0.5) - 1))]


print(f"host: {platform.node()} {platform.machine()} cpus {os.cpu_count()}; os: {platform.platform()}")
print(f"load (1 min) {min(load):.1f} to {max(load):.1f}")
print(f"{'command':<48}{'what':>8}{'n':>6}{'p50':>9}{'p90':>9}{'p99':>9}{'max':>9}")
for i, c in enumerate(a.commands):
    for j, what in enumerate(["ms", "minflt", "majflt"]):
        v = [r[j] for r in rows[i]]
        fmt = "{:>9.3f}" if j == 0 else "{:>9.0f}"
        print(f"{c[-48:]:<48}{what:>8}{len(v):>6}" + "".join(fmt.format(x) for x in [pct(v, 50), pct(v, 90), pct(v, 99), max(v)]))
