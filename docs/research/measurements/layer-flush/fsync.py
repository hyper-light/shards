#!/usr/bin/env python3
"""What a guest's fsync into its writable layer costs (D190): two builds of shards, each with
a home of its own that holds IMAGE already, rounds alternating which goes first. A round runs
a microVM that sleeps, then times `exec` of N 4 KiB writes into its root (its layer), each
fsynced (`dd conv=fsync`), and of the same N without fsync; the difference over N is one
fsync. Then one `sync` of the whole guest, timed.

    fsync.py OLD NEW --homes OLD_HOME,NEW_HOME [--rounds N] [--writes N] [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries; each home holds IMAGE, which is not pulled again (a
registry limits the pulls it serves). The first round is not counted: it makes each build's
template and pool. Prints n, p50, p90, p99 and max of each, with host, OS and revision; with
--raw, each round's, on stderr. Each build's `shards` runs with an empty DOCKER_CONFIG."""
import argparse, atexit, os, platform, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--rounds", type=int, default=10)
p.add_argument("--writes", type=int, default=50)
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="layer-flush-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
builds = {name: (binary, home) for (name, binary), home in zip((("old", a.old), ("new", a.new)), homes)}


@atexit.register
def clean():
    """However the run ends, a failed step's exit included: each build's daemon stopped,
    its runs with it, and this run's own files removed; homes given with --homes are kept."""
    for binary, home in builds.values():
        subprocess.run([binary, "daemon", "stop"], env=dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config),
                       capture_output=True)
    shutil.rmtree(work, ignore_errors=True)


def shards(build, *args):
    binary, home = builds[build]
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    t = time.perf_counter()
    done = subprocess.run([binary, *args], env=e, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    took = (time.perf_counter() - t) * 1000
    if done.returncode != 0:
        raise SystemExit(f"{build} {' '.join(args)}: {done.returncode}: {done.stderr.decode(errors='replace')}")
    return took


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, revision {rev}")
loop = "for i in $(seq {n}); do dd if=/dev/zero of=/written bs=4096 count=1{conv} 2>/dev/null; done"
times = {(b, step): [] for b in builds for step in ("fsync", "sync")}
n = 0
for i in range(a.rounds + 1):
    for build in list(builds) if i % 2 else list(builds)[::-1]:
        n += 1
        name = f"flush-{os.getpid()}-{n}"
        shards(build, "run", "-d", "--name", name, a.image, "sleep", "100000")
        synced = shards(build, "exec", name, "sh", "-c", loop.format(n=a.writes, conv=" conv=fsync"))
        plain = shards(build, "exec", name, "sh", "-c", loop.format(n=a.writes, conv=""))
        whole = shards(build, "exec", name, "sync")
        shards(build, "rm", "-f", name)
        if i > 0:
            times[(build, "fsync")].append((synced - plain) / a.writes)
            times[(build, "sync")].append(whole)
        if a.raw:
            print(f"round {i} {build} with fsync {synced:.1f} ms, without {plain:.1f} ms, sync {whole:.1f} ms",
                  file=sys.stderr)
for (build, step), xs in times.items():
    print(f"{build} {step:<6} n {len(xs)} p50 {pct(xs, .5):8.3f} p90 {pct(xs, .9):8.3f} "
          f"p99 {pct(xs, .99):8.3f} max {max(xs):8.3f} ms")
