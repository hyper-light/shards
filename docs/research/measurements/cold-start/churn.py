#!/usr/bin/env python3
"""Vnode churn: N files under ROOT looked at (`stat`), made once, more than the host's vnode
cache holds (`kern.maxvnodes` on macOS), so that a pass recycles the vnodes of every file no
process holds open: what a build's files cost the host, as kiln's bench met it (PM M315).
Prints how long the pass took.

    churn.py ROOT N
"""
import os, sys, time

root, n = sys.argv[1], int(sys.argv[2])
per = 1000
paths = [os.path.join(root, f"d{i // per}", f"f{i % per}") for i in range(n)]
made = os.path.join(root, f"made-{n}")
if not os.path.exists(made):
    for d in range(0, n, per):
        os.makedirs(os.path.join(root, f"d{d // per}"), exist_ok=True)
    for p in paths:
        os.close(os.open(p, os.O_CREAT | os.O_WRONLY, 0o600))
    open(made, "w").close()
t = time.monotonic()
for p in paths:
    os.stat(p)
print(f"{time.monotonic() - t:.1f} s", end="")
