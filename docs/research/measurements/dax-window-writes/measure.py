#!/usr/bin/env python3
"""What a guest's random 4 KiB writes cost on a volume with a DAX window and without one
(D189: PM M283's question, asked in the guest), its host holding the file's pages or not.

Two homes of one build of shards, each holding IMAGE already: PLAIN's daemon gives a
run's shares no window, DAX's gives each one of WINDOW MiB (SHARDS_SHARE_WINDOW_MIB). Each
round lays out FILE, SIZE MiB of random bytes synced, in DIR (so that its host's file
system holds data, neither holes nor zeros it would compress), and runs the four arms in
an order that turns each round: for each, it leaves the file's pages in the host's page
cache, read whole (`warm`), or not (`cold`: Linux's POSIX_FADV_DONTNEED; on macOS the file
written again with F_NOCACHE, which keeps it out of the unified buffer cache), then times
`run --rm -v DIR:/data IMAGE /data/guestwrite /data/file N BYTES` (guestwrite.c, built
static by zig for the guest's architecture into DIR): N random writes of BYTES (4096
unless --write-size says), each at a multiple of BYTES, with O_DIRECT, the same places each
run; with --premap, a page of each 2 MiB range read first, untimed, so that through a
window each range is mapped before its writes (to be read, which a range's first store
upgrades); with --premap-write, a page of each written first instead (mapped to be
written). Each run's line: its writes' n, p50, p90,
p99 and max, µs, and MiB/s; then, of each arm's rounds, the p50 and the range of each.
With --kernel LABEL=PATH, once for each of two or more guest kernels, every arm is run on
each of them within each round (SHARDS_KERNEL), so that kernels are compared under the
same load, each arm's name ending in its kernel's label.

    measure.py SHARDS --plain HOME --dax HOME --dir DIR [--window MIB] [--size MIB]
               [--writes N] [--write-size BYTES] [--premap | --premap-write] [--rounds N]
               [--image IMAGE] [--kernel LABEL=PATH]...

Each home's daemon is stopped first and last, so that DAX's starts with its window. Each
run's `shards` has an empty DOCKER_CONFIG."""
import argparse, os, platform, shutil, statistics, subprocess, sys, tempfile, time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("--plain", required=True)
p.add_argument("--dax", required=True)
p.add_argument("--dir", required=True)
p.add_argument("--window", type=int, default=4096)
p.add_argument("--size", type=int, default=2048)
p.add_argument("--writes", type=int, default=5000)
p.add_argument("--write-size", type=int, default=4096)
p.add_argument("--premap", action="store_true")
p.add_argument("--premap-write", action="store_true")
p.add_argument("--rounds", type=int, default=5)
p.add_argument("--image", default="alpine")
p.add_argument("--kernel", action="append", default=[])
a = p.parse_args()
kernels = [tuple(k.split("=", 1)) for k in a.kernel] or [("", None)]

here = Path(__file__).resolve().parent
data = Path(a.dir).resolve()
data.mkdir(parents=True, exist_ok=True)
file = data / "file"
config = Path(tempfile.mkdtemp(prefix="dax-writes-"))
macos = sys.platform == "darwin"
F_NOCACHE = 48  # sys/fcntl.h


def env(home, window, kernel=None):
    e = dict(os.environ, SHARDS_HOME=str(Path(home).resolve()), DOCKER_CONFIG=str(config))
    e.pop("SHARDS_SHARE_WINDOW_MIB", None)
    if window:
        e["SHARDS_SHARE_WINDOW_MIB"] = str(window)
    if kernel:
        e["SHARDS_KERNEL"] = kernel
    return e


def stop(home):
    subprocess.run([a.shards, "stop", "daemon"], env=env(home, 0), capture_output=True)


def build():
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}[platform.machine().lower()]
    out = data / "guestwrite"
    subprocess.run(["zig", "cc", "-target", f"{arch}-linux-musl", "-static", "-O2",
                    str(here / "guestwrite.c"), "-o", str(out)], check=True)
    return out


def lay_out():
    chunk = 1 << 20
    with open(file, "wb") as f:
        if macos:
            import fcntl
            fcntl.fcntl(f.fileno(), F_NOCACHE, 1)
        for _ in range(a.size):
            f.write(os.urandom(chunk))
        f.flush()
        os.fsync(f.fileno())


def leave(cache):
    if cache == "warm":
        with open(file, "rb") as f:
            while f.read(1 << 22):
                pass
    elif macos:
        lay_out()
    else:
        fd = os.open(file, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)


ARMS = [("plain", "cold"), ("dax", "cold"), ("plain", "warm"), ("dax", "warm")]
homes = {"plain": (a.plain, 0), "dax": (a.dax, a.window)}


def arm_name(volume, cache, kernel):
    return f"{volume}-{cache}" + (f"-{kernel[0]}" if kernel[0] else "")


def run(volume, kernel):
    home, window = homes[volume]
    done = subprocess.run([a.shards, "run", "--rm", "-v", f"{data}:/data", a.image,
                           "/data/guestwrite", "/data/file", str(a.writes), str(a.write_size)]
                          + (["premap"] if a.premap else ["premapw"] if a.premap_write else []),
                          env=env(home, window, kernel[1]), capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"{volume}: status {done.returncode}\n{done.stdout}{done.stderr}")
    n, p50, p90, p99, mx, rate = done.stdout.split()
    return [int(n), float(p50), float(p90), float(p99), float(mx), float(rate)]


for home, _ in homes.values():
    stop(home)
build()
rev = subprocess.run(["git", "-C", str(here), "rev-parse", "--short", "HEAD"],
                     capture_output=True, text=True).stdout.strip() or "unknown"
load = os.getloadavg()
print(f"host: {platform.node()}, {platform.platform()}, {platform.machine()}; shards {rev}; "
      f"load {load[0]:.2f} at the start")
print(f"{a.size} MiB file, {a.writes} writes of {a.write_size} bytes a run"
      f"{', its ranges mapped first' if a.premap else ''}"
      f"{', its ranges mapped to be written first' if a.premap_write else ''}, window {a.window} MiB; {a.rounds} rounds")
for label, path in kernels:
    if path:
        print(f"kernel {label}: {path}")
print(f"{'round':5} {'arm':14} {'n':>6} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>9} {'MiB/s':>8}")
got = {arm_name(v, c, k): [] for v, c in ARMS for k in kernels}
try:
    for r in range(a.rounds):
        lay_out()
        # Each arm on each kernel in turn, which kernel first turning each round too.
        turned = kernels if r % 2 == 0 else kernels[::-1]
        order = [(v, c, k) for v, c in ARMS[r % len(ARMS):] + ARMS[:r % len(ARMS)] for k in turned]
        for volume, cache, kernel in order:
            leave(cache)
            row = run(volume, kernel)
            got[arm_name(volume, cache, kernel)].append(row)
            print(f"{r:5} {arm_name(volume, cache, kernel):14} {row[0]:6} {row[1]:8.1f} {row[2]:8.1f} "
                  f"{row[3]:8.1f} {row[4]:9.1f} {row[5]:8.1f}", flush=True)
finally:
    for home, _ in homes.values():
        stop(home)
    shutil.rmtree(config, ignore_errors=True)
print(f"load {os.getloadavg()[0]:.2f} at the end")
print("of each arm's rounds: the p50 of each, and its range")
for arm, rows in got.items():
    if not rows:
        continue
    cols = list(zip(*rows))
    said = "  ".join(f"{name} {statistics.median(c):.1f} [{min(c):.1f}, {max(c):.1f}]"
                     for name, c in zip(["p50", "p90", "p99", "max", "MiB/s"], cols[1:]))
    print(f"{arm:14} rounds {len(rows)}: {said}")
