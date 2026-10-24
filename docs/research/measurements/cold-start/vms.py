#!/usr/bin/env python3
"""Each VM's start in a daemon's log (SHARDS_LOG=debug): when it restored, when its
vCPUs were set up and its restore's state dropped, when it was ready, and for one whose
setup took more than 100 ms (or every one with -v) where its setup went: vCPU 0's creation,
prefetch and state, and the prefetch's loop, its CPU time and the process's faults,
page-ins, copy-on-write faults and context switches over it (PM M315).

    vms.py HOME [-v]
"""
import re, sys

home = sys.argv[1]
lines = open(f"{home}/daemon.log", errors="replace").read().splitlines()
vms, cur = [], None
for l in lines:
    if "VM created and RAM mapped" in l:
        cur = [l]
        vms.append(cur)
    elif cur is not None and l.startswith("["):
        cur.append(l)
us = lambda l: int(re.match(r"\[\s*(\d+)us", l).group(1))
for i, v in enumerate(vms):
    get = lambda pat: next((us(l) for l in v if pat in l), None)
    restored, dropped, ready = get("restored a"), get("dropped the restore"), get("ready to run after")
    slow = (dropped or 0) - (restored or 0) > 100_000
    print(f"VM {i:3d}: restored {restored} dropped {dropped} ready {ready}" + ("  <== slow" if slow else ""))
    if slow or "-v" in sys.argv:
        for l in v:
            if any(k in l for k in ("vCPU 0:", "prefetch's loop", "vCPUs set up", "start finished")):
                print("      ", l[l.index("]") + 2:])
