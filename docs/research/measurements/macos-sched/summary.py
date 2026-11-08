#!/usr/bin/env python3
"""failsafe.sh's runs, one line per OUT directory and load: over the busy loops' seconds
(from 3 s to 16 s of the looks), the CPUs the busy VM processes' vCPUs used together, the
share of their busy vCPUs' looks found depressed (base priority 0, XNU's fail-safe) and at
timeshare, and the share of their device workers' looks at real-time; and the host probe's
wakeups late by more than 5 ms then: how many, the latest, and how many by more than
100 ms.

    summary.py OUT...
"""
import collections, glob, os, re, sys

FROM, TO = 3.0, 16.0


def threads(path):
    looks = collections.defaultdict(list)
    for line in open(path):
        t, pid, name, pol, cur, base, ns = line.split()
        looks[(pid, name)].append((float(t), int(pol), int(base), int(ns)))
    return looks


def cpu_between(v):
    inside = [x for x in v if FROM <= x[0] <= TO]
    return (inside[-1][3] - inside[0][3]) / 1e9 if len(inside) > 1 else 0.0


for out in sys.argv[1:]:
    for path in sorted(glob.glob(os.path.join(out, "threads-*.txt"))):
        load = path.rsplit("threads-", 1)[1][:-4]
        looks = threads(path)
        vcpus = {k: v for k, v in looks.items() if re.match(r"vcpu\d+$", k[1])}
        busy = {k: v for k, v in vcpus.items() if cpu_between(v) >= 0.5}
        pids = {k[0] for k in busy}
        used = sum(cpu_between(v) for k, v in vcpus.items() if k[0] in pids) / (TO - FROM)
        states = collections.Counter()
        for v in busy.values():
            for t, pol, base, ns in v:
                if FROM <= t <= TO:
                    states["d" if base == 0 else ("r" if pol == 2 else "t")] += 1
        workers = collections.Counter()
        for (pid, name), v in looks.items():
            if pid in pids and name.startswith("virtio-"):
                for t, pol, base, ns in v:
                    if FROM <= t <= TO:
                        workers["r" if pol == 2 else "t"] += 1
        n = sum(states.values()) or 1
        w = sum(workers.values()) or 1
        late = [float(l.split()[1]) for l in open(os.path.join(out, f"probe-{load}.txt"))
                if FROM <= float(l.split()[0]) <= TO + 1]
        print(f"{out.rstrip('/').rsplit('/', 1)[1]:10} load {load:5} busy vCPUs {len(busy):2} using {used:5.2f} CPUs; "
              f"looks depressed {states['d'] / n:5.1%} timeshare {states['t'] / n:5.1%}; workers real-time "
              f"{workers['r'] / w:5.1%}; probe late >5 ms {len(late):4}, latest {max(late, default=0):6.1f} ms, "
              f">100 ms {sum(x > 100 for x in late)}")
