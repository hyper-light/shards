#!/usr/bin/env python3
"""What a stopped container's writable layer takes of its host's disk once its microVM has
gone, with what its guest freed given back (D190, PM M422) and without: two builds of
shards, each with a home of its own, interleaved. For each size, a container that stays
writes `size` bytes into its root, syncs, removes them and ends; once its layer is kept,
the bytes its file takes on the disk (st_blocks) are read, and NEW's daemon log says what
it gave back and how long that took.

    measure.py OLD NEW [--runs N] [--sizes 0,67108864,1073741824] [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries. Prints n, p50, p90, p99 and max of each, with host, OS
and revision. Each build's `shards` runs with its own SHARDS_HOME (in a temporary
directory) and an empty DOCKER_CONFIG."""
import argparse, atexit, os, platform, re, shutil, subprocess, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--runs", type=int, default=20)
p.add_argument("--sizes", default="0,67108864,1073741824")
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="layer-give-back-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
builds = {name: (binary, os.path.join(work, name)) for name, binary in (("old", a.old), ("new", a.new))}


def env(home):
    return dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)


@atexit.register
def clean():
    for binary, home in builds.values():
        subprocess.run([binary, "daemon", "stop"], env=env(home), capture_output=True)
    shutil.rmtree(work, ignore_errors=True)


def shards(build, *args):
    binary, home = builds[build]
    done = subprocess.run([binary, *args], env=env(home), capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"{build} {' '.join(args)}: {done.returncode}: {done.stderr}")
    return done.stdout


def kept(build, name):
    """The container's layer file once kept, its microVM gone."""
    home = builds[build][1]
    cid = shards(build, "inspect", "-f", "{{.Id}}", name).strip()
    d = os.path.join(home, "containers", cid)
    deadline = time.time() + 60
    while not (os.path.exists(os.path.join(d, "layer.ext4")) and not os.path.exists(os.path.join(d, "layer.new"))):
        if time.time() > deadline:
            raise SystemExit(f"{build} {name}: no layer kept")
        time.sleep(0.02)
    return cid, os.path.join(d, "layer.ext4")


def given(build, cid):
    """What the daemon said it gave back of container `cid`'s layer: bytes, microseconds."""
    log = open(os.path.join(builds[build][1], "daemon.log"), errors="replace").read()
    m = re.findall(rf"container {cid}: gave back (\d+) bytes .* in (\d+) us", log)
    return (int(m[-1][0]), int(m[-1][1])) if m else (0, 0)


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def row(label, xs, unit=""):
    print(f"  {label}: n={len(xs)} p50={pct(xs, .5):.2f}{unit} p90={pct(xs, .9):.2f}{unit} "
          f"p99={pct(xs, .99):.2f}{unit} max={max(xs):.2f}{unit}")


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, revision {rev}")
for build in builds:
    shards(build, "pull", "-q", a.image)
n = 0
for size in [int(s) for s in a.sizes.split(",")]:
    command = f"head -c {size} /dev/zero > /big && sync && rm /big" if size else "true"
    disk = {b: [] for b in builds}
    gave, took = [], []
    for i in range(a.runs + 1):
        for build in list(builds) if i % 2 else list(builds)[::-1]:
            n += 1
            name = f"g-{n}"
            shards(build, "run", "--name", name, a.image, "sh", "-c", command)
            cid, at = kept(build, name)
            # The daemon gives back as it keeps the layer: its log line comes with it.
            time.sleep(0.05)
            st = os.stat(at)
            if i:
                disk[build].append(st.st_blocks * 512 / 2**20)
                if build == "new":
                    g, us = given(build, cid)
                    gave.append(g / 2**20)
                    took.append(us / 1000)
                if a.raw:
                    print(f"    {size} {build} {st.st_blocks * 512} on disk", flush=True)
            shards(build, "rm", "-f", name)
    print(f"size {size}: layer file on disk once kept, MiB")
    for build in builds:
        row(build, disk[build])
    print("  new's give-back:")
    row("given, MiB", gave)
    row("time, ms", took)
