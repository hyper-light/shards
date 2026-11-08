#!/usr/bin/env python3
"""What App Sandbox costs a process's launch (PM M67): the probe signed with App Sandbox
and the hypervisor entitlement, against the same binary signed with the hypervisor
entitlement alone, each spawned to create and destroy a VM and exit. Alternating, after
10 warm-ups each; the host's wall clock from spawn to exit.

    cost.py SANDBOXED UNSANDBOXED [N]
"""
import math, subprocess, sys, time

def pct(v, p):
    v = sorted(v)
    return v[min(max(1, math.ceil(p / 100 * len(v))), len(v)) - 1]

a, b = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 200
times = {a: [], b: []}
for i in range(10 + n):
    for exe in (a, b) if i % 2 == 0 else (b, a):
        t = time.monotonic()
        r = subprocess.run([exe, "hv", "x"], capture_output=True, text=True)
        us = (time.monotonic() - t) * 1e6
        assert r.returncode == 0 and "hv x: ok" in r.stdout, r
        if i >= 10:
            times[exe].append(us)
for exe, label in ((a, "sandboxed"), (b, "unsandboxed")):
    v = times[exe]
    print(f"{label:12} n {len(v)} | p50 {pct(v,50):.0f} | p90 {pct(v,90):.0f} | p99 {pct(v,99):.0f} | max {max(v):.0f} us")
