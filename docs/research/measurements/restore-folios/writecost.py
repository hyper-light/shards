#!/usr/bin/env python3
"""What writing a file a page at a time costs against large runs (D191 restore-folios fix):
for each size, write that many MiB to a fresh file with 2 MiB writes, then with 4 KiB
writes (as write_run does on Linux now), fsync each, timed; rounds alternating. The memory
file a restore maps holds only a booted guest's touched pages (tens of MiB). Prints p50 of
each, ms, and the delta."""
import os, statistics, sys, time
sizes = [int(x) for x in (sys.argv[1].split(",") if len(sys.argv) > 1 else ["64", "256"])]
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 8
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
    os.close(fd); os.unlink(path)
    return dt
for mib in sizes:
    big_ms, pg_ms = [], []
    for i in range(rounds + 1):
        if i % 2 == 0:
            b = once(mib, big, buf_big); p = once(mib, page, buf_pg)
        else:
            p = once(mib, page, buf_pg); b = once(mib, big, buf_big)
        if i:
            big_ms.append(b); pg_ms.append(p)
    bp, pp = statistics.median(big_ms), statistics.median(pg_ms)
    print(f"{mib:>4} MiB: 2MiB-writes {bp:7.2f} ms   4KiB-writes {pp:7.2f} ms   delta {pp-bp:+7.2f} ms   ({mib*256} pages)")
