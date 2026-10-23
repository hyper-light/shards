#!/usr/bin/env python3
"""What keeping a stopped container's writable layer in its own file costs (D190, PM M278),
against packing it as a tar archive as its microVM stops (D37, PM M115, M116): two builds
of shards, each with a home of its own, interleaved so that the host's load falls on both
alike. For each size, a container that stays writes `size` bytes into its root and ends
(`run`: what its stop costs beside the client's run), then `start -a` runs it again over
its files (its layer put back), `diff` reads its files stopped (a visit), and `commit`
makes an image of them.

    ab.py OLD NEW [--runs N] [--sizes 0,4194304,67108864] [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries. Prints n, p50, p90, p99 and max of each, in milliseconds,
with host, OS and revision; with --raw, each round's times too, on stderr. Each build's
`shards` runs with its own SHARDS_HOME (beside this script's temporary directory) and an
empty DOCKER_CONFIG."""
import argparse, atexit, os, platform, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--runs", type=int, default=20)
p.add_argument("--sizes", default="0,4194304,67108864")
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="layer-kept-")
config = os.path.join(work, "docker-config")
os.makedirs(config)


def env(home):
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    return e


builds = {name: (binary, os.path.join(work, name)) for name, binary in (("old", a.old), ("new", a.new))}


@atexit.register
def clean():
    """However the run ends, a failed step's exit included: each build's daemon stopped,
    its runs with it, and this run's own files removed."""
    for binary, home in builds.values():
        subprocess.run([binary, "daemon", "stop"], env=dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config),
                       capture_output=True)
    shutil.rmtree(work, ignore_errors=True)


def shards(build, *args, check=True):
    binary, home = builds[build]
    t = time.perf_counter()
    done = subprocess.run([binary, *args], env=env(home), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    took = (time.perf_counter() - t) * 1000
    if check and done.returncode != 0:
        raise SystemExit(f"{build} {' '.join(args)}: {done.returncode}: {done.stderr.decode(errors='replace')}")
    return took


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, revision {rev}")
for build in builds:
    shards(build, "pull", "-q", a.image)
n = 0
for size in [int(s) for s in a.sizes.split(",")]:
    command = f"head -c {size} /dev/zero > /written" if size else "true"
    times = {(b, step): [] for b in builds for step in ("run", "start", "diff", "commit")}
    # One round of each first, warming templates and pools, not counted.
    for i in range(a.runs + 1):
        # Each build first in turn.
        for build in list(builds) if i % 2 else list(builds)[::-1]:
            n += 1
            name = f"kept-{n}"
            took = {"run": shards(build, "run", "--name", name, a.image, "sh", "-c", command)}
            took["start"] = shards(build, "start", "-a", name)
            took["diff"] = shards(build, "diff", name)
            took["commit"] = shards(build, "commit", name, f"kept:{n}")
            shards(build, "rm", "-f", name)
            shards(build, "rmi", f"kept:{n}")
            if i > 0:
                for step, ms in took.items():
                    times[(build, step)].append(ms)
            if a.raw:
                print(f"size {size} round {i} {build} " + " ".join(f"{k} {v:.2f}" for k, v in took.items()),
                      file=sys.stderr)
    for (build, step), xs in times.items():
        print(f"size {size:>9} {build} {step:<6} n {len(xs)} p50 {pct(xs, .5):8.2f} p90 {pct(xs, .9):8.2f} "
              f"p99 {pct(xs, .99):8.2f} max {max(xs):8.2f} ms")
