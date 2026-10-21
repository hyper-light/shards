#!/usr/bin/env python3
"""A run's start in two builds of shards, paced, with each run's own timing line (D191):
`run --rm [FLAGS] IMAGE true` in each build in turn, each 0.3 s after the last so that its
warm microVMs are made again between runs, with SHARDS_TIMING set; the client's whole
time, and from the VM's timing line its guest's phases: from the request to the command's
start, the command's run, and from its exit to its answer.

    paced.py OLD NEW --homes OLD_HOME,NEW_HOME [--runs N] [--image IMAGE] -- [FLAGS...]

OLD and NEW are `shards` binaries; each home holds IMAGE, which is not pulled again. Three
runs of each first are not counted. Prints n, p50 and p90 in milliseconds, with host, OS,
load and revision. Each build's `shards` runs with an empty DOCKER_CONFIG."""
import argparse, json, os, platform, statistics, subprocess, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--runs", type=int, default=30)
p.add_argument("--image", default="alpine")
p.add_argument("flags", nargs="*")
a = p.parse_args()

config = os.path.join(tempfile.mkdtemp(prefix="cpu-paced-"), "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
builds = {name: (binary, home) for (name, binary), home in zip((("old", a.old), ("new", a.new)), homes)}
# shards_abi::marker: the workload's command executing, and its main process exited.
STARTED, EXITED = 4, 5


def run(build):
    binary, home = builds[build]
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config, SHARDS_TIMING="1")
    t = time.perf_counter()
    done = subprocess.run([binary, "run", "--rm", *a.flags, a.image, "true"], env=e, capture_output=True, text=True)
    took = (time.perf_counter() - t) * 1000
    if done.returncode != 0:
        raise SystemExit(f"{build}: {done.returncode}: {done.stderr}")
    line = next((l for l in done.stderr.splitlines() if l.startswith("shards-timing ")), None)
    return took, json.loads(line.split(" ", 1)[1]) if line else None


def at(timing, key):
    if isinstance(key, str):
        return timing.get(key) or None
    return next((t for (m, t) in timing["markers"] if m == key), None)


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {os.cpu_count()} CPUs, {platform.system()} "
      f"{platform.release()}, revision {rev}, load {os.getloadavg()}, flags {a.flags}")
for build in builds:
    for _ in range(3):
        run(build)
rows = {b: [] for b in builds}
for i in range(a.runs):
    # Each build first in turn.
    for build in list(builds) if i % 2 else list(builds)[::-1]:
        rows[build].append(run(build))
        time.sleep(0.3)
for build, done in rows.items():
    out = [f"{build} client n {len(done)} p50 {statistics.median(t for t, _ in done):.2f} "
           f"p90 {sorted(t for t, _ in done)[int(.9 * len(done))]:.2f}"]
    for name, start, end in (("request to start", "request_us", STARTED), ("command", STARTED, EXITED),
                             ("exit to answer", EXITED, "answered_us")):
        spans = sorted((at(j, end) - at(j, start)) / 1000 for _, j in done
                       if j and at(j, start) and at(j, end))
        if spans:
            out.append(f"{name} p50 {statistics.median(spans):.3f} p90 {spans[int(.9 * len(spans))]:.3f}")
    print("; ".join(out))
print(f"load {os.getloadavg()}")
