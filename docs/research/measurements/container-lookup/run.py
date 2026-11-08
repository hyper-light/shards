#!/usr/bin/env python3
"""How long a container command takes to find its container among many, for two builds,
interleaved: `shards port NAME` (found by name) and `shards port PREFIX` (found by the
start of its ID), each of a container with no ports, so the answer is the lookup and
little else. Each build gets a home of its own holding CONTAINERS exited containers'
records, which its daemon reads as it starts; the first command of each starts it and is
not counted.

    run.py A_BIN_DIR B_BIN_DIR CONTAINERS RUNS SCRATCH_DIR

A_BIN_DIR and B_BIN_DIR each hold a release `shards`, `shardsd` and `shards-vm` (a
build is known by its daemon and VM binaries together).
"""
import json
import os
import platform
import random
import secrets
import shutil
import statistics
import subprocess
import sys
import time


def home_with(path, containers):
    """A home whose `containers` holds `containers` exited containers' records."""
    shutil.rmtree(path, ignore_errors=True)
    root = os.path.join(path, "containers")
    os.makedirs(root, mode=0o700)
    os.chmod(path, 0o700)
    now = time.time_ns()
    ids = []
    for i in range(containers):
        cid = secrets.token_hex(32)
        ids.append(cid)
        d = os.path.join(root, cid)
        os.mkdir(d, 0o700)
        record = {
            "id": cid, "name": f"c-{i}", "image": "img", "command": ["true"],
            "created": now - i, "state": "exited", "started": now - i, "finished": now - i,
            "exit_code": 0, "auto_remove": False,
        }
        with open(os.path.join(d, "config.json"), "w") as f:
            json.dump(record, f)
    return ids


def timed(bin_dir, home, args):
    env = dict(os.environ, SHARDS_HOME=home)
    t0 = time.perf_counter_ns()
    out = subprocess.run([os.path.join(bin_dir, "shards"), *args], env=env,
                         capture_output=True)
    took = time.perf_counter_ns() - t0
    if out.returncode != 0:
        raise SystemExit(f"{args}: {out.returncode}: {out.stderr.decode()}")
    return took


def summary(ns):
    ns = sorted(ns)
    pick = lambda q: ns[min(len(ns) - 1, int(q * len(ns)))]
    return (f"n={len(ns)} p50={pick(0.5) / 1e6:.2f} ms p90={pick(0.9) / 1e6:.2f} ms "
            f"p99={pick(0.99) / 1e6:.2f} ms max={ns[-1] / 1e6:.2f} ms")


def main():
    a_bin, b_bin, containers, runs, scratch = sys.argv[1:6]
    containers, runs = int(containers), int(runs)
    builds = {"A": a_bin, "B": b_bin}
    homes, ids = {}, {}
    for k in builds:
        homes[k] = os.path.join(scratch, f"lookup-{k}")
        ids[k] = home_with(homes[k], containers)
        timed(builds[k], homes[k], ["ps", "-q"])
    rng = random.Random(1)
    times = {(k, how): [] for k in builds for how in ("name", "prefix")}
    for _ in range(runs):
        i = rng.randrange(containers)
        for k in rng.sample(sorted(builds), 2):
            times[(k, "name")].append(timed(builds[k], homes[k], ["port", f"c-{i}"]))
            times[(k, "prefix")].append(timed(builds[k], homes[k], ["port", ids[k][i][:12]]))
    rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True,
                         text=True).stdout.strip()
    print(f"host {platform.node()} {platform.machine()} {platform.platform()}, rev {rev}, "
          f"{containers} containers")
    for (k, how), ns in sorted(times.items()):
        print(f"{k} {how:6} {summary(ns)}")
    for k in builds:
        env = dict(os.environ, SHARDS_HOME=homes[k])
        subprocess.run([os.path.join(builds[k], "shards"), "daemon", "stop"], env=env,
                       capture_output=True)
        shutil.rmtree(homes[k], ignore_errors=True)


if __name__ == "__main__":
    main()
