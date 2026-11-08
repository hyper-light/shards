#!/usr/bin/env python3
"""threads.py's looks at VM processes, per BIN seconds, over every VM process whose vCPUs
used half a second of CPU or more: for its vCPUs and for its device workers (virtio-*),
how many looks found a thread at the real-time policy (2), at timeshare (1), and depressed
(base priority 0: XNU's fail-safe, osfmk/kern/thread_policy.c DEPRESSPRI), and the CPUs'
worth of time they used. Then each busy vCPU's runs of looks at one state (r real-time,
t timeshare, d depressed), to see the fail-safe, or the governor, move them.

    timeline.py THREADS [BIN]
"""
import collections, re, sys

path = sys.argv[1]
bin_s = float(sys.argv[2]) if len(sys.argv) > 2 else 1.0
looks = collections.defaultdict(list)
for line in open(path):
    t, pid, name, pol, cur, base, ns = line.split()
    looks[(pid, name)].append((float(t), int(pol), int(base), int(ns)))
vcpu = lambda name: re.match(r"vcpu\d+$", name) is not None
used = collections.Counter()
for (pid, name), v in looks.items():
    if vcpu(name):
        used[pid] += v[-1][3] - v[0][3]
busy = sorted(p for p, ns in used.items() if ns >= 5e8)
group = lambda name: "vcpus" if vcpu(name) else ("workers" if name.startswith("virtio-") else None)
state = lambda pol, base: "d" if base == 0 else ("r" if pol == 2 else "t")
bins = collections.defaultdict(lambda: collections.defaultdict(lambda: collections.Counter()))
cpu = collections.defaultdict(lambda: collections.defaultdict(float))
for (pid, name), v in looks.items():
    g = group(name)
    if pid not in busy or g is None:
        continue
    for i, (t, pol, base, ns) in enumerate(v):
        bins[int(t // bin_s)][g][state(pol, base)] += 1
        if i:
            # Each look's CPU time since the last, spread over the time between them.
            t0, ns0 = v[i - 1][0], v[i - 1][3]
            cpu[int(t // bin_s)][g] += (ns - ns0) / 1e9 / bin_s
print(f"busy VM processes {busy}; per {bin_s} s: per group, looks real-time/timeshare/depressed, CPUs used")
for b in sorted(bins):
    cells = " ".join(f"{g} {c['r']}/{c['t']}/{c['d']} {cpu[b][g]:5.2f}" for g, c in sorted(bins[b].items()))
    print(f"{b * bin_s:5.1f} {cells}")
print("busy vCPUs' runs of looks (state x looks @ first second):")
for (pid, name), v in sorted(looks.items()):
    if pid not in busy or not vcpu(name) or v[-1][3] - v[0][3] < 5e8:
        continue
    runs = []
    for t, pol, base, ns in v:
        s = state(pol, base)
        if runs and runs[-1][0] == s:
            runs[-1][1] += 1
        else:
            runs.append([s, 1, t])
    print(f"  {pid} {name}: " + " ".join(f"{s}x{n}@{t:.1f}" for s, n, t in runs))
