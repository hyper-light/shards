#!/usr/bin/env python3
"""What probe.py recorded, by endpoint: the requests, their answer times (n, p50, p90, p99,
max), the failures by kind, and each episode of failures: from its first failed request's
start to its first answer's, how many requests it took, and when it began, in seconds
from the first request (PM M486).

    episodes.py PROBE.jsonl
"""
import json
import sys
from collections import Counter, defaultdict


def failed(r):
    return r["status"] is None or r["status"] == 429 or r["status"] >= 500


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else float("nan")


def main():
    by = defaultdict(list)
    for line in open(sys.argv[1]):
        r = json.loads(line)
        by[(r["host"], r["path"].split("?")[0])].append(r)
    total = sum(len(v) for v in by.values())
    first = min(r["t"] for v in by.values() for r in v)
    last = max(r["t"] for v in by.values() for r in v)
    print(f"{total} requests over {(last - first) / 3600:.2f} h")
    episodes = []
    for (host, path), rs in sorted(by.items()):
        rs.sort(key=lambda r: r["t"])
        took = [r["took"] * 1000 for r in rs if not failed(r)]
        kinds = Counter(str(r["status"] or r["error"]) for r in rs if failed(r))
        print(f"{host}{path}: n {len(rs)}, answered in ms p50 {q(took, .5):.0f} p90 {q(took, .9):.0f} "
              f"p99 {q(took, .99):.0f} max {max(took) if took else float('nan'):.0f}; failed {dict(kinds)}")
        start = None
        asked = 0
        for r in rs:
            # `t` is taken as the answer came: the request began `took` before it.
            began = r["t"] - r["took"]
            if failed(r):
                if start is None:
                    start, asked, what = began, 0, str(r["status"] or r["error"])
                asked += 1
            elif start is not None:
                episodes.append((r["t"] - start, asked, host, path, what, start))
                start = None
        if start is not None:
            episodes.append((rs[-1]["t"] - start, asked, host, path, what + " (unended)", start))
    print(f"{len(episodes)} episodes")
    for length, asked, host, path, what, start in sorted(episodes, reverse=True):
        print(f"  {length:8.3f} s  {asked:4d} asks  {host}{path}  {what}  from +{start - first:.0f} s")


if __name__ == "__main__":
    main()
