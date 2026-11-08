#!/usr/bin/env python3
"""What a memory-heavy run costs in two builds of shards (D191's slice 1, a run's microVM
with every CPU of its host, against the build before it, whose `--cpus 1` microVM had one),
each with a home of its own that holds IMAGE already, rounds alternating which goes first.
Each round, for each build, times from the request to the exit:

- `run --rm --cpus 1 IMAGE true`: a start alone;
- `run --rm -m 700m --cpus 1 --tmpfs /big:size=800m IMAGE dd ... count=684` and the same
  of `-m 2500m` and 2484 MiB: its guest plugs and touches that much memory, the tmpfs's
  pages charged to its cgroup, as tests/containers.rs `run_limits_a_microvms_resources`'s
  `alloc` does (the x86 CI flake, b96501d3: its 2500m case passed 60 s on a contended
  runner).

With --load N, N busy loops run throughout, on the CPUs this runs on (run it under
`taskset` to choose them: its runs inherit them).

    memory.py OLD NEW --homes OLD_HOME,NEW_HOME --revs OLD_REV,NEW_REV [--rounds N]
              [--warmup N] [--load N] [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries; each home holds IMAGE, which is not pulled again (a
registry limits the pulls it serves). The first --warmup rounds (1) are not counted: they
boot and save each build's template. Prints n, p50, p90, p99 and max of each, with host,
OS, kernel, CPUs, load and revisions; a run that fails is counted apart, with its error;
with --raw, each round's, on stderr. Each build's `shards` runs with an empty
DOCKER_CONFIG."""
import argparse, os, platform, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--revs", required=True)
p.add_argument("--rounds", type=int, default=8)
p.add_argument("--warmup", type=int, default=1)
p.add_argument("--load", type=int, default=0)
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="cpu-memory-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
revs = a.revs.split(",")
builds = {"old": (a.old, homes[0], revs[0]), "new": (a.new, homes[1], revs[1])}


def fill(mib, limit):
    return ["--rm", "-m", f"{limit}m", "--cpus", "1", "--tmpfs", f"/big:size={limit + 100}m",
            a.image, "dd", "if=/dev/zero", "of=/big/x", "bs=1M", f"count={mib}"]


CASES = [
    ("start", ["--rm", "--cpus", "1", a.image, "true"]),
    ("fill 684 MiB at -m 700m", fill(684, 700)),
    ("fill 2484 MiB at -m 2500m", fill(2484, 2500)),
]


def run(build, args):
    binary, home, _ = builds[build]
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    t = time.perf_counter()
    done = subprocess.run([binary, "run", *args], env=e, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    ms = (time.perf_counter() - t) * 1000
    err = None if done.returncode == 0 else f"exit {done.returncode}: {done.stderr.decode(errors='replace').strip()[-300:]}"
    return ms, err


def stats(xs):
    # As ab.py takes them.
    xs = sorted(xs)
    at = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    return len(xs), at(0.5), at(0.9), at(0.99), xs[-1]


def cpuinfo():
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor()


def os_name():
    try:
        for line in open("/etc/os-release"):
            if line.startswith("PRETTY_NAME="):
                return line.split("=", 1)[1].strip().strip('"')
    except OSError:
        pass
    return platform.system()


def reset():
    """Each home's daemon stopped and its templates removed, as ab.py does: a daemon keeps
    the CPUs it started on, and a template the vCPUs of the host it was made on."""
    for binary, home, _ in builds.values():
        e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
        subprocess.run([binary, "daemon", "stop"], env=e, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        templates = os.path.join(home, "templates")
        if os.path.isdir(templates):
            shutil.rmtree(templates)


reset()
loops = [subprocess.Popen(["sh", "-c", "while :; do :; done"]) for _ in range(a.load)]
load0 = os.getloadavg()
times = {(b, c): [] for b in builds for c, _ in CASES}
failed = {(b, c): [] for b in builds for c, _ in CASES}
try:
    for r in range(a.warmup + a.rounds):
        order = ["old", "new"] if r % 2 == 0 else ["new", "old"]
        for b in order:
            for name, args in CASES:
                ms, err = run(b, args)
                if r < a.warmup:
                    continue
                if err:
                    failed[(b, name)].append(err)
                else:
                    times[(b, name)].append(ms)
                if a.raw:
                    print(f"round {r - a.warmup} {b} {name}: {ms:.1f} ms{' ' + err if err else ''}", file=sys.stderr)
finally:
    for l in loops:
        l.kill()
    for l in loops:
        l.wait()
    reset()
    shutil.rmtree(work, ignore_errors=True)

cpus = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
print(f"host: {platform.node()}, {cpuinfo()}; {os_name()}, kernel {platform.release()}; "
      f"{cpus} CPUs this ran on; {a.load} busy loops; load {load0[0]:.2f} at the start, "
      f"{os.getloadavg()[0]:.2f} at the end")
print(f"old {builds['old'][2]}, new {builds['new'][2]}; image {a.image}; {a.rounds} rounds "
      f"after {a.warmup} warmup, alternating")
print(f"{'case':28} {'build':5} {'n':>3} {'p50':>9} {'p90':>9} {'p99':>9} {'max':>9}  ms")
for name, _ in CASES:
    for b in builds:
        xs = times[(b, name)]
        if xs:
            n, p50, p90, p99, mx = stats(xs)
            print(f"{name:28} {b:5} {n:3} {p50:9.1f} {p90:9.1f} {p99:9.1f} {mx:9.1f}")
        for err in failed[(b, name)]:
            print(f"{name:28} {b:5} FAILED {err}")
