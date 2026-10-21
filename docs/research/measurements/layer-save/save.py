#!/usr/bin/env python3
"""What a save of a running microVM costs (D176), with its writable layer (D190), in two
builds of shards, each with a home of its own that holds IMAGE already, rounds alternating
which goes first. For each size, a microVM writes `size` random bytes into its root (its
layer) and sleeps; `save vm --leave-running` saves it and lets it go on (timed), and `save
vm` saves it again and stops it (timed); then it is removed.

    save.py OLD NEW --homes OLD_HOME,NEW_HOME [--rounds N] [--sizes 0,67108864]
            [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries; each home holds IMAGE, which is not pulled again (a
registry limits the pulls it serves). One round of each size first is not counted: it
warms the image's template and pools. Prints n, p50, p90, p99 and max of each, in
milliseconds, with host, OS and revision; with --raw, each round's, on stderr. Each build's
`shards` runs with an empty DOCKER_CONFIG."""
import argparse, os, platform, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--rounds", type=int, default=10)
p.add_argument("--sizes", default="0,67108864")
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="layer-save-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
builds = {name: (binary, home) for (name, binary), home in zip((("old", a.old), ("new", a.new)), homes)}


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
n = 0
for size in [int(s) for s in a.sizes.split(",")]:
    fill = f"head -c {size} /dev/urandom > /written && sync && " if size else ""
    command = f"{fill}echo done > /done && sleep 100000"
    times = {(b, step): [] for b in builds for step in ("save, left running", "save, stopped")}
    for i in range(a.rounds + 1):
        # Each build first in turn.
        for build in list(builds) if i % 2 else list(builds)[::-1]:
            n += 1
            name = f"save-{n}"
            shards(build, "run", "-d", "--name", name, a.image, "sh", "-c", command)
            written(build, name, size)
            took = {
                "save, left running": shards(build, "save", "vm", name, "running", "--leave-running")[0],
                "save, stopped": shards(build, "save", "vm", name, "stopped")[0],
            }
            shards(build, "rm", "-f", name)
            if i > 0:
                for step, ms in took.items():
                    times[(build, step)].append(ms)
            if a.raw:
                print(f"size {size} round {i} {build} " + ", ".join(f"{k} {v:.2f}" for k, v in took.items()),
                      file=sys.stderr)
    for (build, step), xs in times.items():
        print(f"size {size:>9} {build} {step:<19} n {len(xs)} p50 {pct(xs, .5):9.2f} p90 {pct(xs, .9):9.2f} "
              f"p99 {pct(xs, .99):9.2f} max {max(xs):9.2f} ms")
for build in builds:
    shards(build, "daemon", "stop", check=False)
