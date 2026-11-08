"""E6's comparison of a run.sh OUT_DIR, guest against host, over its rounds: each matrix
multiplication of test-backend-ops, its time per run (the median of its rounds) on each
side and their ratio, with the cases' geometric mean; llama-bench's tokens/s, n, p50, p90
and max of every round's repetitions; and the steps of host-visible memory, the median
of the rounds' p50 and p99, in microseconds.

    python3 compare.py OUT_DIR
"""

import json
import math
import re
import sys
from pathlib import Path

CASE = re.compile(r"^\s*(MUL_MAT\(.*?\)):\s+(\d+) runs -\s+([\d.]+) us/run")


def cases(path):
    out = {}
    for line in path.read_text(errors="replace").splitlines():
        m = CASE.match(re.sub(r"\x1b\[[0-9;]*m", "", line))
        if m:
            out[m.group(1)] = float(m.group(3))
    return out


def bench(path):
    out = {}
    for t in json.loads(path.read_text()):
        test = f"pp{t['n_prompt']}" if t["n_prompt"] else f"tg{t['n_gen']}"
        out[test] = sorted(t["samples_ts"])
    return out


def blobs(path):
    out = {}
    for line in path.read_text().splitlines():
        words = line.split()
        if len(words) == 7 and words[2].startswith("n="):
            out[(words[0], int(words[1]))] = (float(words[3][4:]), float(words[5][4:]))
    return out


def median(v):
    v = sorted(v)
    return v[len(v) // 2]


def rounds(out, side, kind, ext, read):
    """Each round's file of `side` and `kind`, read and pooled by key."""
    pooled = {}
    for path in sorted(out.glob(f"{side}-{kind}-*.{ext}")):
        for key, value in read(path).items():
            pooled.setdefault(key, []).append(value)
    return pooled


def main(out):
    guest = {c: median(v) for c, v in rounds(out, "guest", "mul-mat", "txt", cases).items()}
    host = {c: median(v) for c, v in rounds(out, "host", "mul-mat", "txt", cases).items()}
    both = [c for c in guest if c in host]
    ratios = [guest[c] / host[c] for c in both]
    print(f"MUL_MAT cases in both: {len(both)}")
    for c in both:
        print(f"  {guest[c] / host[c]:5.2f}x  guest {guest[c]:9.2f} us  host {host[c]:9.2f} us  {c[:110]}")
    if ratios:
        geo = math.exp(sum(math.log(r) for r in ratios) / len(ratios))
        s = sorted(ratios)
        print(f"guest/host time: geometric mean {geo:.3f}, p50 {s[len(s) // 2]:.3f}, max {s[-1]:.3f}")
    pool = lambda side: {t: sorted(x for r in v for x in r) for t, v in rounds(out, side, "bench", "json", bench).items()}
    g, h = pool("guest"), pool("host")
    for test in g:
        for side, v in (("guest", g[test]), ("host", h.get(test, []))):
            if v:
                q = lambda p: v[min(len(v) - 1, len(v) * p // 100)]
                print(f"{test} {side:5} n={len(v)} p50={q(50):.1f} p90={q(90):.1f} min={v[0]:.1f} max={v[-1]:.1f} tokens/s")
    steps = lambda side: {
        k: (median(p50 for p50, _ in v), median(p99 for _, p99 in v)) for k, v in rounds(out, side, "blobs", "txt", blobs).items()
    }
    gb, hb = steps("guest"), steps("host")
    for key in gb:
        if key in hb:
            print(f"{key[0]:8} {key[1]:9} guest p50 {gb[key][0]:7.1f} p99 {gb[key][1]:7.1f}  host p50 {hb[key][0]:7.1f} p99 {hb[key][1]:7.1f} us")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
