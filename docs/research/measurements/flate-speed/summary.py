"""PM M256: ab.sh's lines to n, p50, p90, p99 and max per build and file, and whether
each file's outputs were the same bytes. Usage: python3 summary.py < ab.txt"""
import re
import sys

times, digests = {}, {}
for line in sys.stdin:
    m = re.match(r"(old|new) (\S+): n=1 p50=([0-9.]+)s .* sha256=([0-9a-f]+)", line)
    if not m:
        continue
    which, path, t, digest = m.group(1), m.group(2).rsplit("/", 1)[-1], float(m.group(3)), m.group(4)
    times.setdefault((path, which), []).append(t)
    digests.setdefault(path, set()).add(digest)


def at(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, int(q * len(v)))]


for (path, which), v in sorted(times.items()):
    print(f"{path} {which}: n={len(v)} p50={at(v, .5):.3f}s p90={at(v, .9):.3f}s "
          f"p99={at(v, .99):.3f}s max={max(v):.3f}s")
for path, d in sorted(digests.items()):
    print(f"{path}: {'the same bytes' if len(d) == 1 else 'DIFFERENT bytes'}")
