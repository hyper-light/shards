#!/usr/bin/env python3
"""What a visit to a stopped microVM's files costs (D37, daemon/visit.rs): `shards diff`
of a stopped microVM, which boots a VM over its files, against `shards diff` of a running
one, interleaved, each timed from the client's start to its exit.

    visit.py SHARDS IMAGE [N]

SHARDS_HOME is a fresh directory of this script's, into which it pulls IMAGE
first, with an empty DOCKER_CONFIG; both are removed as it ends. Prints n, p50, p90, p99
and max in ms for each.
"""
import atexit, os, shutil, subprocess, sys, tempfile, time

def main():
    shards, image = sys.argv[1], sys.argv[2]
    n = int(sys.argv[3]) if len(sys.argv) > 3 else 100
    work = tempfile.mkdtemp(prefix="shards-visit-")
    home, config = os.path.join(work, "home"), os.path.join(work, "docker-config")
    os.makedirs(home)
    os.makedirs(config)
    env = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    run = lambda *a, **k: subprocess.run([shards, *a], env=env, capture_output=True, **k)

    @atexit.register
    def clean():
        """However it ends, a failed diff's exit included: its daemon stopped, its runs
        with it, and its files removed."""
        run("daemon", "stop")
        shutil.rmtree(work, ignore_errors=True)

    run("pull", "-q", image, check=True)
    run("run", "--name", "stopped", image, "true", check=True)
    run("run", "-d", "--name", "running", image, "sleep", "100000", check=True)
    times = {"running": [], "stopped": []}
    for i in range(n + 5):
        for name in ("running", "stopped"):
            t = time.perf_counter()
            r = run("diff", name)
            dt = (time.perf_counter() - t) * 1000
            if r.returncode != 0:
                sys.exit(f"{name}: {r.stderr.decode()}")
            if i >= 5:  # warm-up
                times[name].append(dt)
    for name, ts in times.items():
        ts.sort()
        q = lambda p: ts[min(len(ts) - 1, int(p * len(ts)))]
        print(f"diff {name}: n={len(ts)} p50={q(.5):.2f} p90={q(.9):.2f} p99={q(.99):.2f} max={ts[-1]:.2f} ms")

main()
