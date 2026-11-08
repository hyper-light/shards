#!/usr/bin/env python3
"""What writing a file a page at a time costs against large runs (D25, the restore-folios
fix, PM M290): for each size, write that many MiB to a fresh file with 2 MiB writes, then
with 4 KiB writes (as write_run does on Linux now), fsync each, timed; rounds alternating
which goes first, the first round not counted. The memory file a restore maps holds only a
booted guest's touched pages (tens of MiB). Prints n, p50, p90, p99 and max of each, ms
(percentiles as cpu-elastic/ab.py takes them), and the delta of the p50s, with host, OS,
kernel and the file system written to.

    writecost.py [SIZES_MIB] [ROUNDS] [DIR]    (defaults: 64,256  20  ~)"""
import os, platform, subprocess, sys, time

sizes = [int(x) for x in (sys.argv[1].split(",") if len(sys.argv) > 1 else ["64", "256"])]
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 20
d = sys.argv[3] if len(sys.argv) > 3 else os.path.expanduser("~")
page, big = 4096, 2 << 20
buf_big, buf_pg = b"\xab" * big, b"\xab" * page


def once(mib, chunk, buf):
    path = os.path.join(d, f".writecost-{os.getpid()}.bin")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    total = mib << 20
    t = time.perf_counter()
    off = 0
    while off < total:
        n = min(chunk, total - off)
        os.pwrite(fd, buf[:n], off)
        off += n
    os.fsync(fd)
    dt = (time.perf_counter() - t) * 1000
    os.close(fd)
    os.unlink(path)
    return dt


def stats(xs):
    xs = sorted(xs)
    at = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    return len(xs), at(0.5), at(0.9), at(0.99), xs[-1]


def first(path, key):
    try:
        for line in open(path):
            if line.startswith(key):
                return line.split("=" if "=" in key else ":", 1)[1].strip().strip('"')
    except OSError:
        pass
    return "?"


try:
    fs = subprocess.run(["findmnt", "-no", "FSTYPE", "-T", d], capture_output=True, text=True).stdout.strip()
except OSError:
    fs = ""
print(f"host: {platform.node()}, {first('/proc/cpuinfo', 'model name')}; "
      f"{first('/etc/os-release', 'PRETTY_NAME=')}, kernel {platform.release()}; "
      f"{d} on {fs or '?'}; {rounds} rounds after 1, alternating")
print(f"{'size':>8} {'writes':>7} {'n':>3} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>8}  ms")
for mib in sizes:
    big_ms, pg_ms = [], []
    for i in range(rounds + 1):
        if i % 2 == 0:
            b = once(mib, big, buf_big)
            p = once(mib, page, buf_pg)
        else:
            p = once(mib, page, buf_pg)
            b = once(mib, big, buf_big)
        if i:
            big_ms.append(b)
            pg_ms.append(p)
    for name, xs in (("2 MiB", big_ms), ("4 KiB", pg_ms)):
        n, p50, p90, p99, mx = stats(xs)
        print(f"{mib:>4} MiB {name:>7} {n:3} {p50:8.2f} {p90:8.2f} {p99:8.2f} {mx:8.2f}")
    print(f"{mib:>4} MiB   delta of the p50s: {stats(pg_ms)[1] - stats(big_ms)[1]:+.2f} ms ({mib * 256} pages)")
