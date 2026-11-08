#!/usr/bin/env python3
"""Reads setup-costs.sh's output (D189, PM M302): for each arm, its writes' p50 (the median
of its rounds' and their range), and each FUSE_SETUPMAPPING's parts its VM process logged
(setup-costs.patch), n, p50, p90, p99 and max in µs over all its rounds: the descriptor,
from the share process or a copy of the one the window holds, and the whole; the file's
size, the host mapping, stage 2, the table, the held record, and the settling, whose
watch only a file's first mapping makes.

    setup-costs.py OUT"""
import re
import statistics
import sys
from collections import defaultdict


def summary(v):
    v = sorted(v)
    at = lambda p: v[min(max(int(-(-p * len(v) // 1)), 1), len(v)) - 1]
    return f"n {len(v)} | p50 {at(0.5)} | p90 {at(0.9)} | p99 {at(0.99)} | max {v[-1]}"


writes = defaultdict(list)
parts = defaultdict(lambda: defaultdict(list))
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) < 3 or not f[0].isdigit():
        if f:
            print(line.rstrip())
        continue
    arm = f[1]
    if f[2] == "WRITES":
        # guestwrite's stdout, n and p50 first, wherever its stderr's classes fell among it.
        said = re.sub(r"(first|later) (0|\d+ [\d.]+ [\d.]+ [\d.]+ [\d.]+);?", " ", " ".join(f[3:])).split()
        writes[arm].append(float(said[1]))
        continue
    m = re.search(r"SETUP-COST node \d+ write (\d)(?: reused (\d))? descriptor (\d+) total (\d+)", line)
    if m:
        whence = "the window's copy" if m[2] == "1" else "from its server"
        parts[arm][f"descriptor, {whence}"].append(int(m[3]))
        parts[arm]["setup, whole"].append(int(m[4]))
        continue
    m = re.search(r"MAP-COST node \d+ size (\d+) mmap (\d+) stage2 (\d+) insert (\d+) held (\d+) "
                  r"settle (\d+) watch (\d)", line)
    if m:
        for name, value in zip(["size", "mmap", "stage2", "table", "held"], m.groups()[:5]):
            parts[arm][name].append(int(value))
        parts[arm]["settle, watched" if m[7] == "1" else "settle"].append(int(m[6]))
for arm in sorted(set(writes) | set(parts)):
    w = writes.get(arm, [])
    if w:
        print(f"{arm}: writes p50 {statistics.median(w):.1f} [{min(w):.1f}, {max(w):.1f}], {len(w)} rounds")
    for name, values in parts[arm].items():
        print(f"  {name}: {summary(values)} us")
