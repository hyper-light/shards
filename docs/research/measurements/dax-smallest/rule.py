#!/usr/bin/env python3
"""measure.py's decision rule (its docstring's, fixed before any number was taken) applied
to one host's output: for each measured size, whether the window arm's per-file p50 and p99
are no worse than the FUSE arm's beyond the rounds' spread for first reads (cold and warm),
re-reads (`again`) and overwrites, both caches each; whether the VM process's memory per
mapped file is no more than the FUSE arm's; and, from the sets run with --set-smallest,
whether the sets at a threshold failed no request, kept within the host's limits and took
no longer than through FUSE. The rounds' spread is each arm's range of its per-round
values: the window arm is worse beyond it where its whole range lies above the FUSE arm's,
and no worse where the two overlap or its range lies below. A size passes where every
comparison does; the rule's S is the smallest size that passes with every larger size
passing too. Several outputs (one host's passes) may be given: a size must pass in each.

    rule.py OUT...
"""
import re
import statistics
import sys
from collections import defaultdict

PHASES = ("first", "again", "overwrite")
SUMMARY = re.compile(
    r"^\s*(\S+) (\w+)\s+(cold|warm) (fuse|window)\s+rounds (\d+) files (\d+): "
    r"p50 ([\d.]+) \[([\d.]+), ([\d.]+)\]\s+p99 ([\d.]+) \[([\d.]+), ([\d.]+)\]")
ROUND = re.compile(r"^round \d+ (fuse|window) (\S+) (cold|warm) ")
SET = re.compile(r"^set (\w+) \((\d+) files\) round \d+ (fuse|window) window (\S+) smallest (\S+) "
                 r"load [\d.]+ run ([\d.]+)s")
UNITS = {"k": 1 << 10, "m": 1 << 20}


def size_of(label):
    return int(label[:-1]) * UNITS[label[-1]] if label[-1] in UNITS else int(label)


def read(path):
    cells, memory, sets, failed, refused = {}, defaultdict(list), defaultdict(list), [], 0
    cur = None
    for line in open(path, errors="replace"):
        m = SUMMARY.match(line)
        if m:
            size, phase, cache, arm = m.group(1), m.group(2), m.group(3), m.group(4)
            cells[(size, phase, cache, arm)] = {
                "p50": (float(m.group(7)), float(m.group(8)), float(m.group(9))),
                "p99": (float(m.group(10)), float(m.group(11)), float(m.group(12)))}
            continue
        m = ROUND.match(line)
        if m:
            cur = (m.group(2), m.group(1))
            continue
        if cur and line.startswith("  setups"):
            got = re.search(r"memory_kib (\d+)", line)
            if got:
                memory[cur].append(int(got.group(1)))
            got = re.search(r"refused (\d+)", line)
            refused += int(got.group(1)) if got else 0
            cur = None
            continue
        m = SET.match(line)
        if m:
            name, arm, window, smallest, took = m.group(1), m.group(3), m.group(4), m.group(5), float(m.group(6))
            refused_here = re.search(r"refused (\d+)", line)
            sets[(name, window, arm, smallest)].append((took, int(refused_here.group(1)) if refused_here else 0))
        if line.startswith("FAILED"):
            failed.append(line.strip())
    return cells, memory, sets, failed, refused


def worse_beyond_spread(window, fuse):
    """The window arm's whole range of per-round values above the FUSE arm's."""
    return window[1] > fuse[2]


def main(paths):
    verdict = {}
    for path in paths:
        cells, memory, sets, failed, refused = read(path)
        sizes = sorted({k[0] for k in cells}, key=size_of)
        print(f"== {path}: sizes {', '.join(sizes)}; runs failed {len(failed)}; matrix mappings refused {refused}")
        files = None
        for size in sizes:
            reasons = []
            for phase in PHASES:
                for cache in ("cold", "warm"):
                    w, f = cells.get((size, phase, cache, "window")), cells.get((size, phase, cache, "fuse"))
                    if not w or not f:
                        reasons.append(f"{phase} {cache}: missing")
                        continue
                    for q in ("p50", "p99"):
                        if worse_beyond_spread(w[q], f[q]):
                            reasons.append(f"{phase} {cache} {q} window [{w[q][1]}, {w[q][2]}] above fuse "
                                           f"[{f[q][1]}, {f[q][2]}]")
            mw, mf = memory.get((size, "window")), memory.get((size, "fuse"))
            if mw and mf:
                per = (statistics.median(mw) - statistics.median(mf)) / 1000
                if per > 0:
                    reasons.append(f"memory: window {per:+.1f} KiB a file over fuse (medians, 1,000 files)")
            else:
                reasons.append("memory: missing")
            ok = not reasons
            verdict.setdefault(size, []).append(ok)
            print(f"  {size:>6}: {'passes' if ok else 'fails'}" + ("" if ok else ": " + "; ".join(reasons)))
        if sets:
            print("  sets (run seconds, median [range]; refused mappings):")
            for key in sorted(sets, key=lambda k: (k[0], k[1], k[2], int(k[3]) if k[3] != "-" else -1)):
                took = [t for t, _ in sets[key]]
                print(f"    {key[0]:5} window {key[1]:>7} {key[2]:6} smallest {key[3]:>8}: "
                      f"{statistics.median(took):.2f} [{min(took):.2f}, {max(took):.2f}] "
                      f"refused {sum(r for _, r in sets[key])}")
            for (name, window, arm, smallest), runs in sorted(sets.items()):
                if arm != "window":
                    continue
                fuse = [t for t, _ in sets.get((name, window, "fuse", "-"), [])]
                took = [t for t, _ in runs]
                if fuse:
                    slower = min(took) > max(fuse)
                    print(f"    {name} window {window} at {smallest}: "
                          f"{'longer than FUSE beyond the spread' if slower else 'no longer than FUSE'}")
    passing = [s for s in sorted(verdict, key=size_of) if all(verdict[s])]
    ordered = sorted(verdict, key=size_of)
    smallest = None
    for i, s in enumerate(ordered):
        if all(all(verdict[t]) for t in ordered[i:]):
            smallest = s
            break
    print(f"sizes passing in every output: {', '.join(passing) or '-'}; the rule's S by the matrix: {smallest or 'none'}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    main(sys.argv[1:])
