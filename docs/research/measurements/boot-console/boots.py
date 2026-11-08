#!/usr/bin/env python3
"""PM M376: what a cold boot's console costs. Boots the pinned kernel with no root, to its
panic and reset, N times each way, the ways interleaved: its console as the daemon's guests
have it (`console=ttyS0 earlycon panic=-1`, every message the kernel prints written to the
serial port, each byte a port or MMIO exit), and told `quiet`, which writes only warnings
and worse. Each boot's time from its start to its VM process's end, the guest's own time
at its panic, and the bytes its console wrote; n, p50, p90, p99 and max of each.

Usage: boots.py SHARDS KERNEL [N] [VCPUS]
"""

import os
import re
import subprocess
import sys
import tempfile
import time

shards, kernel = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 20
vcpus = sys.argv[4] if len(sys.argv) > 4 else str(os.cpu_count())
ways = {"plain": "", "quiet": " quiet"}
took = {w: [] for w in ways}
guest = {w: [] for w in ways}
wrote = {w: [] for w in ways}
# No docker config of this host's is read (the tests' rule for manual runs).
env = dict(os.environ, DOCKER_CONFIG=tempfile.mkdtemp(prefix="m376-"))
for _ in range(n):
    for way, told in ways.items():
        t0 = time.monotonic()
        p = subprocess.run(
            [shards, "run", "--kernel", kernel, "--cpus", vcpus, "--memory", "512",
             "--cmdline", "console=ttyS0 earlycon panic=-1" + told],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env,
        )
        took[way].append(time.monotonic() - t0)
        out = p.stdout.decode(errors="replace")
        wrote[way].append(len(p.stdout))
        at = re.findall(r"^\[\s*(\d+\.\d+)\] Kernel panic", out, re.M)
        guest[way].append(float(at[0]) if at else float("nan"))


def summary(values, unit, scale):
    v = sorted(values)
    pick = lambda q: v[min(len(v) - 1, int(q * len(v)))] * scale
    return (f"n={len(v)} p50={pick(0.5):.1f} p90={pick(0.9):.1f} p99={pick(0.99):.1f} "
            f"max={v[-1] * scale:.1f} {unit}")


print(f"M376 {os.uname().sysname} {os.uname().machine}, {vcpus} vCPUs, {n} boots each way")
for way in ways:
    print(f"M376 {way}, each in order: " + " ".join(f"{t * 1e3:.0f}" for t in took[way]) + " ms")
for way in ways:
    print(f"M376 {way}: boot to its VM's end {summary(took[way], 'ms', 1e3)}; "
          f"guest at its panic {summary(guest[way], 'ms', 1e3)}; "
          f"console {summary(wrote[way], 'bytes', 1)}")
