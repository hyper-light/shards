#!/usr/bin/env python3
"""Save and restore of a running microVM (D176), from the caller's view.

bench.py SHARDS N [IMAGE]

Each of N rounds runs a microVM of IMAGE (alpine:3.22) counting every millisecond, and:
- saves it, left running: the client's time for `save vm --leave-running`, and the guest's
  pause, its longest gap between counts (`logs -t`) around the save;
- saves it, stopped: the client's time for `save vm`;
- restores it: the client's time for `restore vm`, until it runs again;
then removes it. Prints n, p50, p90, p99 and max of each, in ms, and the snapshots' sizes.
The home is SHARDS_HOME's; SHARDS_KERNEL and SHARDS_INIT as the caller sets them (run.sh).
"""
import datetime
import os
import subprocess
import sys
import time

SHARDS = sys.argv[1]
N = int(sys.argv[2])
IMAGE = sys.argv[3] if len(sys.argv) > 3 else "alpine:3.22"
COUNTER = "i=0; while :; do i=$((i+1)); echo $i; usleep 1000; done"


def shards(*args, check=True):
    t0 = time.monotonic()
    done = subprocess.run([SHARDS, *args], capture_output=True, text=True)
    took = (time.monotonic() - t0) * 1000
    if check and done.returncode != 0:
        sys.exit(f"shards {' '.join(args)}: {done.returncode}\n{done.stdout}{done.stderr}")
    return done, took


def counts(name):
    """(time, count) of each count its log holds, by `logs -t`."""
    out, _ = shards("logs", "-t", name)
    got = []
    for line in out.stdout.splitlines():
        stamp, _, rest = line.partition(" ")
        if rest.isdigit():
            got.append((stamp_ns(stamp), int(rest)))
    return got


def stamp_ns(stamp):
    """RFC 3339 with nanoseconds, as `logs -t` writes it, in nanoseconds since the epoch."""
    base, _, frac = stamp.rstrip("Z").partition(".")
    whole = datetime.datetime.strptime(base, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
    return int(whole.timestamp()) * 1_000_000_000 + int(frac.ljust(9, "0")[:9] or 0)


def wait_counts(name, at_least):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        got = counts(name)
        if len(got) >= at_least:
            return got
        time.sleep(0.05)
    sys.exit(f"{name}: fewer than {at_least} counts")


def stats(xs):
    xs = sorted(xs)
    pick = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    return f"n={len(xs)} p50={pick(0.5):.1f} p90={pick(0.9):.1f} p99={pick(0.99):.1f} max={xs[-1]:.1f}"


left, pause, stopped, restored, sizes = [], [], [], [], []
for i in range(N):
    name = f"bench-{os.getpid()}-{i}"
    shards("run", "-d", "--name", name, IMAGE, "sh", "-c", COUNTER)
    wait_counts(name, 200)
    before = len(counts(name))
    _, took = shards("save", "vm", name, "running", "--leave-running")
    left.append(took)
    got = wait_counts(name, before + 500)
    # The guest's pause: its longest gap between counts, from just before the save on.
    window = got[max(0, before - 50):]
    gaps = [(b[0] - a[0]) / 1e6 for a, b in zip(window, window[1:])]
    pause.append(max(gaps))
    _, took = shards("save", "vm", name, "stopped")
    stopped.append(took)
    _, took = shards("restore", "vm", name)
    restored.append(took)
    wait_counts(name, len(counts(name)) + 50)
    listed, _ = shards("list", "snapshots", name)
    sizes.extend(line.split()[-1] for line in listed.stdout.splitlines()[1:])
    shards("rm", "-f", name)

print(f"save --leave-running, client ms: {stats(left)}")
print(f"save --leave-running, guest's pause ms: {stats(pause)}")
print(f"save, stopped, client ms: {stats(stopped)}")
print(f"restore, client ms: {stats(restored)}")
print(f"snapshot sizes on disk: {sorted(set(sizes))}")
