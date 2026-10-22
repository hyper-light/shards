#!/usr/bin/env python3
"""Where a run's start goes, in two builds of shards (D191, PM M292: a `--cpus 1` run on a
contended host starts 2.2 times as slowly since slice 1), each with a home of its own that
holds IMAGE already, rounds alternating which goes first. Each round, for each build,
--starts runs of `run --rm --cpus 1 IMAGE true` with SHARDS_TIMING set, whose client
prints its VM's timing line: the VM process's clock readings, in µs since it started,
and its guest's markers (shards_abi::control::marker). From each, besides the client's
wall time:

- `restore`: the VM process's start to its guest connected for a workload (CONNECTED),
  and where the request came before that, the part of it the request waited for (`waited`);
- `start`: the request to the workload's command executing (WORKLOAD_STARTED);
- `command`: the command's run (WORKLOAD_STARTED to WORKLOAD_EXITED);
- `answer`: its exit to the VM's answer (`answered_us`).

With a build that writes markers 101 to 106 through init's start of a run
(`markers.patch`), each step between them too: `m101` the request to init's taking it,
`m102` its limits, `m103` the rest of its setup to the orders sent to the standby, `m104`
the standby's waking with them, `m105` its own setup to its exec, `m106` init's waking as
the exec is made, `m4` its listener and agents to WORKLOAD_STARTED.

With --load N, N busy loops run throughout, on the CPUs this runs on (run it under
`taskset`: its runs inherit them).

    phases.py OLD NEW --homes OLD_HOME,NEW_HOME --revs OLD_REV,NEW_REV [--rounds N]
              [--starts N] [--warmup N] [--load N] [--image IMAGE]

Each home's daemon is stopped and its templates removed first and last (as ab.py does).
Prints n, p50, p90, p99 and max of each, ms (percentiles as ab.py takes them), with host,
OS, kernel, CPUs, load and revisions. Each build's `shards` runs with an empty
DOCKER_CONFIG."""
import argparse, json, os, platform, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--revs", required=True)
p.add_argument("--rounds", type=int, default=10)
p.add_argument("--starts", type=int, default=5)
p.add_argument("--warmup", type=int, default=1)
p.add_argument("--load", type=int, default=0)
p.add_argument("--image", default="alpine")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="cpu-phases-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
revs = a.revs.split(",")
builds = {"old": (a.old, homes[0], revs[0]), "new": (a.new, homes[1], revs[1])}
CONNECTED, STARTED, EXITED = 3, 4, 5
PHASES = ["wall", "restore", "waited", "start", "command", "answer"]
# The steps between markers.patch's markers, in order, where a build writes them.
STEPS = [101, 102, 103, 104, 105, 106, STARTED]


def env(home, **more):
    return dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config, **more)


def reset():
    for binary, home, _ in builds.values():
        subprocess.run([binary, "daemon", "stop"], env=env(home), capture_output=True)
        templates = os.path.join(home, "templates")
        if os.path.isdir(templates):
            shutil.rmtree(templates)


def once(build):
    binary, home, _ = builds[build]
    t = time.perf_counter()
    done = subprocess.run([binary, "run", "--rm", "--cpus", "1", a.image, "true"],
                          env=env(home, SHARDS_TIMING="1"), capture_output=True, text=True)
    wall = (time.perf_counter() - t) * 1000
    if done.returncode != 0:
        return None, f"exit {done.returncode}: {done.stderr.strip()[-200:]}"
    line = next((l for l in done.stderr.splitlines() if l.startswith("shards-timing ")), None)
    if line is None:
        return None, "no timing line"
    j = json.loads(line.split(" ", 1)[1])
    marks = {m: t for m, t in j.get("markers", [])}
    if not all(k in marks for k in (CONNECTED, STARTED, EXITED)) or not j.get("request_us"):
        return None, f"markers missing: {line[:200]}"
    request, answered = j["request_us"], j["answered_us"]
    steps = {}
    if all(m in marks for m in STEPS):
        at = request
        for m in STEPS:
            steps[f"m{m}"] = (marks[m] - at) / 1000
            at = marks[m]
    return steps | {
        "wall": wall,
        "restore": marks[CONNECTED] / 1000,
        "waited": max(0, marks[CONNECTED] - request) / 1000,
        "start": (marks[STARTED] - request) / 1000,
        "command": (marks[EXITED] - marks[STARTED]) / 1000,
        "answer": (answered - marks[EXITED]) / 1000,
    }, None


def stats(xs):
    xs = sorted(xs)
    at = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    return len(xs), at(0.5), at(0.9), at(0.99), xs[-1]


def first(path, key):
    try:
        for line in open(path):
            if line.startswith(key):
                return line.split("=" if "=" in key else ":", 1)[1].strip().strip('"')
    except OSError:
        pass
    return "?"


reset()
loops = [subprocess.Popen(["sh", "-c", "while :; do :; done"]) for _ in range(a.load)]
load0 = os.getloadavg()
got = {b: {k: [] for k in PHASES + [f"m{m}" for m in STEPS]} for b in builds}
errors = []
try:
    for r in range(a.warmup + a.rounds):
        for b in (["old", "new"] if r % 2 == 0 else ["new", "old"]):
            for _ in range(a.starts):
                phases, err = once(b)
                if r < a.warmup:
                    continue
                if err:
                    errors.append(f"{b}: {err}")
                    continue
                for k, v in phases.items():
                    got[b][k].append(v)
finally:
    for l in loops:
        l.kill()
    for l in loops:
        l.wait()
    reset()
    shutil.rmtree(work, ignore_errors=True)

cpus = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
print(f"host: {platform.node()}, {first('/proc/cpuinfo', 'model name')}; "
      f"{first('/etc/os-release', 'PRETTY_NAME=')}, kernel {platform.release()}; {cpus} CPUs this ran on; "
      f"{a.load} busy loops; load {load0[0]:.2f} at the start, {os.getloadavg()[0]:.2f} at the end")
print(f"old {builds['old'][2]}, new {builds['new'][2]}; image {a.image}; {a.rounds} rounds of "
      f"{a.starts} starts after {a.warmup} warmup, alternating")
print(f"{'phase':9} {'build':5} {'n':>4} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>8}  ms")
for k in PHASES + [f"m{m}" for m in STEPS]:
    for b in builds:
        if got[b][k]:
            n, p50, p90, p99, mx = stats(got[b][k])
            print(f"{k:9} {b:5} {n:4} {p50:8.2f} {p90:8.2f} {p99:8.2f} {mx:8.2f}")
for e in errors:
    print("FAILED", e)
