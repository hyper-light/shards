#!/usr/bin/env python3
"""Whether a host's filesystem stalls the calls a share's server makes, with no microVM: the
calls npm's install makes of a volume, as the share process makes them (server.rs), timed
one by one in batches, a batch every --every seconds for --minutes, each batch in a fresh
directory under DIR: --files files each made with open(2) O_CREAT|O_WRONLY|O_EXCL, given
--size bytes by pwrite(2), and close(2)d, then all unlinked and the directory removed.
Each batch's line: its start, the host's load, and for each call n, p50, p99 and max in µs,
with the batch's whole time. A filesystem whose calls stall at times shows batches whose
p99 and max are far past the rest.

    fsprobe.py DIR [--files 2000] [--size 8192] [--every 5] [--minutes 10] [--out OUT]
"""
import argparse, os, statistics, time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("dir")
p.add_argument("--files", type=int, default=2000)
p.add_argument("--size", type=int, default=8192)
p.add_argument("--every", type=float, default=5)
p.add_argument("--minutes", type=float, default=10)
p.add_argument("--out", default="")
a = p.parse_args()
root = Path(a.dir)
root.mkdir(parents=True, exist_ok=True)
out = open(a.out, "a") if a.out else None
data = os.urandom(a.size)


def say(line):
    print(line, flush=True)
    if out:
        out.write(line + "\n")
        out.flush()


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(len(v) * f))]


def batch(i):
    d = root / f"b{os.getpid()}-{i}"
    d.mkdir()
    took = {"open": [], "pwrite": [], "close": [], "unlink": []}
    t0 = time.perf_counter_ns()
    for n in range(a.files):
        path = str(d / f"f{n:05}")
        t = time.perf_counter_ns()
        fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_EXCL, 0o644)
        u = time.perf_counter_ns()
        os.pwrite(fd, data, 0)
        v = time.perf_counter_ns()
        os.close(fd)
        w = time.perf_counter_ns()
        took["open"].append((u - t) / 1e3)
        took["pwrite"].append((v - u) / 1e3)
        took["close"].append((w - v) / 1e3)
    for n in range(a.files):
        t = time.perf_counter_ns()
        os.unlink(d / f"f{n:05}")
        took["unlink"].append((time.perf_counter_ns() - t) / 1e3)
    d.rmdir()
    whole = (time.perf_counter_ns() - t0) / 1e9
    calls = "; ".join(f"{k} n {len(v)} p50 {statistics.median(v):.0f} p99 {q(v, .99):.0f} max {max(v):.0f}"
                      for k, v in took.items())
    return whole, calls


say(f"fsprobe: {a.files} files of {a.size} bytes a batch under {root}, a batch every {a.every} s for {a.minutes} min")
end = time.monotonic() + a.minutes * 60
i = 0
wholes = []
while time.monotonic() < end:
    start = time.monotonic()
    load = os.getloadavg()[0]
    whole, calls = batch(i)
    wholes.append(whole)
    say(f"{time.strftime('%H:%M:%S')} batch {i} load {load:.1f} whole {whole:.2f}s; {calls}")
    i += 1
    time.sleep(max(0.0, a.every - (time.monotonic() - start)))
if wholes:
    say(f"batches {len(wholes)}: whole p50 {statistics.median(wholes):.2f}s p90 {q(wholes, .9):.2f}s max {max(wholes):.2f}s")
