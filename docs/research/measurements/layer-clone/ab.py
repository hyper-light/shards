#!/usr/bin/env python3
"""What a restore's clone of its snapshot's writable layer saves (D190), against the copy it
made before: two builds of shards, each with a home of its own, interleaved so that the
host's load falls on both alike. For each size, a microVM writes `size` random bytes into
its root (random: the copy skips blocks of zeros) and sleeps; `save vm` saves and stops it
(D176), and `restore vm` resumes it from that snapshot, timed until it returns, its copy or
clone of the layer made on the way; then it is removed. A `run` of a container that ends
at once, between, has its warm microVM restored from its image's template ahead of it, as
every pool's is.

    ab.py OLD NEW [--runs N] [--sizes 0,67108864,536870912] [--image IMAGE] [--raw]
          [--homes OLD_HOME,NEW_HOME]

OLD and NEW are `shards` binaries. Prints n, p50, p90, p99 and max of each, in milliseconds,
with host, OS and revision; with --raw, each round's times too, on stderr. Each build's
`shards` runs with its own SHARDS_HOME (beside this script's temporary directory, or the
homes --homes names, which hold IMAGE already and are not pulled into again: a registry
limits the pulls it serves) and an empty DOCKER_CONFIG."""
import argparse, atexit, os, platform, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--runs", type=int, default=10)
p.add_argument("--sizes", default="0,67108864,536870912")
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
p.add_argument("--homes")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="layer-clone-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = a.homes.split(",") if a.homes else [os.path.join(work, "old"), os.path.join(work, "new")]
builds = {name: (binary, home) for (name, binary), home in zip((("old", a.old), ("new", a.new)), homes)}


@atexit.register
def clean():
    """However the run ends, a failed step's exit included: each build's daemon stopped,
    its runs with it, and this run's own files removed; homes given with --homes are kept."""
    for binary, home in builds.values():
        subprocess.run([binary, "daemon", "stop"], env=dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config),
                       capture_output=True)
    shutil.rmtree(work, ignore_errors=True)


def shards(build, *args, check=True):
    binary, home = builds[build]
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    t = time.perf_counter()
    done = subprocess.run([binary, *args], env=e, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    took = (time.perf_counter() - t) * 1000
    if check and done.returncode != 0:
        raise SystemExit(f"{build} {' '.join(args)}: {done.returncode}: {done.stderr.decode(errors='replace')}")
    return took, done.stdout.decode(errors="replace")


def written(build, name, size):
    """Waits until the microVM's root holds the file it writes, whole."""
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        _, out = shards(build, "exec", name, "sh", "-c", "cat /done 2>/dev/null", check=False)
        if out.strip() == "done":
            return
        time.sleep(0.05)
    raise SystemExit(f"{build} {name}: {size} bytes never written")


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, revision {rev}")
if not a.homes:
    for build in builds:
        shards(build, "pull", "-q", a.image)
n = 0
for size in [int(s) for s in a.sizes.split(",")]:
    fill = f"head -c {size} /dev/urandom > /written && sync && " if size else ""
    command = f"{fill}echo done > /done && sleep 100000"
    times = {(b, step): [] for b in builds for step in ("restore", "run")}
    # One round of each first, warming templates and pools, not counted.
    for i in range(a.runs + 1):
        # Each build first in turn.
        for build in list(builds) if i % 2 else list(builds)[::-1]:
            n += 1
            name = f"clone-{n}"
            shards(build, "run", "-d", "--name", name, a.image, "sh", "-c", command)
            written(build, name, size)
            shards(build, "save", "vm", name, "saved")
            took = {"restore": shards(build, "restore", "vm", name, "saved")[0]}
            shards(build, "rm", "-f", name)
            took["run"] = shards(build, "run", "--rm", a.image, "true")[0]
            if i > 0:
                for step, ms in took.items():
                    times[(build, step)].append(ms)
            if a.raw:
                print(f"size {size} round {i} {build} " + " ".join(f"{k} {v:.2f}" for k, v in took.items()),
                      file=sys.stderr)
    for (build, step), xs in times.items():
        print(f"size {size:>9} {build} {step:<7} n {len(xs)} p50 {pct(xs, .5):8.2f} p90 {pct(xs, .9):8.2f} "
              f"p99 {pct(xs, .99):8.2f} max {max(xs):8.2f} ms")
