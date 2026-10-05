#!/usr/bin/env python3
"""What keeping a container's writable layer costs a run (D37): `shards run` of a
container that stays, which saves its layer as it stops, against `--rm`, which saves
none, interleaved so that the host's load falls on both alike. Each run writes `size`
bytes into its root first, so the layer holds them.

    ab.py [--runs N] [--sizes 0,4194304] [--image alpine]

Prints n, p50, p90, p99 and max of each, in milliseconds, with host, OS and revision."""
import argparse, os, platform, statistics, subprocess, time

p = argparse.ArgumentParser()
p.add_argument("--runs", type=int, default=50)
p.add_argument("--sizes", default="0,4194304")
p.add_argument("--image", default="alpine")
a = p.parse_args()

def run(remove, size):
    cmd = ["shards", "run"] + (["--rm"] if remove else []) + [a.image, "sh", "-c",
           f"head -c {size} /dev/zero > /written" if size else "true"]
    t = time.perf_counter()
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL)
    return (time.perf_counter() - t) * 1000

def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, revision {rev}")
for size in [int(s) for s in a.sizes.split(",")]:
    # Warm the template and pool first.
    run(True, size); run(False, size)
    kept, removed = [], []
    for _ in range(a.runs):
        removed.append(run(True, size))
        kept.append(run(False, size))
    subprocess.run("shards ps -aq --filter status=exited >/dev/null 2>&1; shards container prune -f >/dev/null",
                   shell=True)
    for name, xs in (("--rm (no layer)", removed), ("kept (layer saved)", kept)):
        print(f"size {size:>8} {name:<20} n {len(xs)} p50 {pct(xs, .5):7.2f} p90 {pct(xs, .9):7.2f} "
              f"p99 {pct(xs, .99):7.2f} max {max(xs):7.2f} ms")
