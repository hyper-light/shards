#!/usr/bin/env python3
"""A microVM's I/O limits (D178), from its guest's writes to a volume.

bench.py SHARDS N [IMAGE]

Each of N rounds runs, in turn, three microVMs of IMAGE (alpine:3.22), each writing to a
volume with busybox dd, synced at its end, between two lines its log times (`logs -t`):
- `paced`: 16 MiB under --io-maxbandwidth 4m, its achieved rate the bytes over the time
  between the lines, as a share of the limit;
- `free` and `high`: 64 MiB with no limit, and under one no write comes near (1 GiB a
  second), interleaved: what the limiter costs a request it does not hold back.
Prints n, p50, p90, p99 and max of each. The home is SHARDS_HOME's.
"""
import datetime
import subprocess
import sys

SHARDS = sys.argv[1]
N = int(sys.argv[2])
IMAGE = sys.argv[3] if len(sys.argv) > 3 else "alpine:3.22"
MIB = 1 << 20


def shards(*args):
    done = subprocess.run([SHARDS, *args], capture_output=True, text=True)
    if done.returncode != 0:
        sys.exit(f"shards {' '.join(args)}: {done.returncode}\n{done.stdout}{done.stderr}")
    return done.stdout


def stamp_ns(stamp):
    """RFC 3339 with nanoseconds, as `logs -t` writes it, in nanoseconds since the epoch."""
    base, _, frac = stamp.rstrip("Z").partition(".")
    whole = datetime.datetime.strptime(base, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
    return int(whole.timestamp()) * 1_000_000_000 + int(frac.ljust(9, "0")[:9] or 0)


def written(name, limits, mib):
    """Seconds its guest took to write and sync MIB MiB, between its two lines."""
    script = f"echo start; dd if=/dev/zero of=/v/{name} bs=65536 count={mib * 16} conv=fsync 2>/dev/null; echo end"
    shards("run", "--pull", "never", "--name", name, "-u", "0", "-v", "iob:/v", *limits, IMAGE, "sh", "-c", script)
    at = {}
    for line in shards("logs", "-t", name).splitlines():
        stamp, _, rest = line.partition(" ")
        if rest in ("start", "end"):
            at[rest] = stamp_ns(stamp)
    shards("rm", name)
    return (at["end"] - at["start"]) / 1e9


def summary(what, unit, xs):
    xs = sorted(xs)
    pick = lambda q: xs[min(len(xs) - 1, int(len(xs) * q))]
    print(f"{what}, {unit}: n={len(xs)} p50={pick(0.5):.3f} p90={pick(0.9):.3f} p99={pick(0.99):.3f} max={xs[-1]:.3f}")


shards("pull", "-q", IMAGE)
paced, free, high = [], [], []
for i in range(N):
    took = written(f"paced{i}", ["--io-maxbandwidth", "4m"], 16)
    paced.append(16 * MIB / took / (4 * MIB))
    free.append(written(f"free{i}", [], 64))
    high.append(written(f"high{i}", ["--io-maxbandwidth", "1g"], 64))
summary("paced at 4 MiB/s, achieved over limit", "ratio", paced)
summary("64 MiB, no limit", "s", free)
summary("64 MiB, limit of 1 GiB/s", "s", high)
