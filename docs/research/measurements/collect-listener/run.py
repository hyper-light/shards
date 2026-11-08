#!/usr/bin/env python3
"""Whether a collection holds up a daemon's clients (review 7.14), for two builds,
alternating: each round leaves FILES files in the store's `ingest/`, which a collection
removes, marks a collection due (`images/collect-due`, as a pull leaves it), and times
`shards ps` from then, one after another, until the files are gone. A collection run on
the thread that accepts clients keeps every client waiting in its backlog until it ends.
Reported per build: each `ps`'s wall clock, and the collection's own time, which its
daemon logs.

    run.py A_BIN_DIR B_BIN_DIR FILES ROUNDS SCRATCH_DIR

Each BIN_DIR holds a build's `shards`, `shardsd` and `shards-vm`, signed as
scripts/hvf-run signs them; no VM is started.
"""
import os
import platform
import re
import shutil
import subprocess
import sys
import time


def shards(bin_dir, home, *args):
    env = dict(os.environ, SHARDS_HOME=home)
    t0 = time.perf_counter_ns()
    out = subprocess.run([os.path.join(bin_dir, "shards"), *args], env=env,
                         capture_output=True)
    took = time.perf_counter_ns() - t0
    if out.returncode != 0:
        raise SystemExit(f"shards {' '.join(args)}: {out.returncode}: {out.stderr.decode()}")
    return took


def summary(ns):
    ns = sorted(ns)
    pick = lambda q: ns[min(len(ns) - 1, int(q * len(ns)))]
    return (f"n={len(ns)} p50={pick(0.5) / 1e6:.2f} ms p90={pick(0.9) / 1e6:.2f} ms "
            f"p99={pick(0.99) / 1e6:.2f} ms max={ns[-1] / 1e6:.2f} ms")


def round_of(bin_dir, home, files):
    ingest = os.path.join(home, "images", "ingest")
    os.makedirs(ingest, exist_ok=True)
    for i in range(files):
        with open(os.path.join(ingest, f"left-{i}"), "wb") as f:
            f.write(b"x")
    marker = os.path.join(ingest, "left-0")
    with open(os.path.join(home, "images", "collect-due"), "wb"):
        pass
    times = []
    deadline = time.time() + 120
    while os.path.exists(marker) or not times:
        if time.time() > deadline:
            raise SystemExit("the collection never ran")
        times.append(shards(bin_dir, home, "ps", "-q"))
    return times


def main():
    a_bin, b_bin, files, rounds, scratch = sys.argv[1:6]
    files, rounds = int(files), int(rounds)
    builds = {"A": a_bin, "B": b_bin}
    homes = {}
    for k, b in builds.items():
        homes[k] = os.path.join(scratch, f"collect-{k}")
        shutil.rmtree(homes[k], ignore_errors=True)
        os.makedirs(homes[k], mode=0o700)
        # The daemon, its store, and its first collection, at its start.
        shards(b, homes[k], "ps", "-q")
        time.sleep(1)
    times = {k: [] for k in builds}
    for r in range(rounds):
        for k in (sorted(builds) if r % 2 == 0 else sorted(builds, reverse=True)):
            times[k].extend(round_of(builds[k], homes[k], files))
    rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True,
                         text=True).stdout.strip()
    print(f"host {platform.node()} {platform.machine()} {platform.platform()}, rev {rev}, "
          f"{files} files a round, {rounds} rounds, load {os.getloadavg()[0]:.2f}")
    for k in sorted(builds):
        log = open(os.path.join(homes[k], "daemon.log")).read()
        took = [m for m in re.findall(r"files left in ingest/: \d+ bytes, in ([^\s]+)", log)]
        print(f"{k} ps {summary(times[k])}; collections took {', '.join(took[-rounds:])}")
        env = dict(os.environ, SHARDS_HOME=homes[k])
        subprocess.run([os.path.join(builds[k], "shards"), "daemon", "stop"], env=env,
                       capture_output=True)
        shutil.rmtree(homes[k], ignore_errors=True)


if __name__ == "__main__":
    main()
