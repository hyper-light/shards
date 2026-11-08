#!/usr/bin/env python3
"""The restore-folios experiment's tables, from its runs' logs (`gh run view ID --log`):
each run's kernel, then, for each arm, the restored memory file and first beat of the
Firecracker comparison and a 16-VM fleet's PSS a VM, and the restore and image benches'
rows: every round's p50 (and the comparison's max of the memory file), as PM M291 gives
them.

    summarize.py LOG...

Rows are the benches' own: p50, p90, p99 and max, n as the workflow ran them (20 a
comparison, 30 a restore bench, 60 an image bench)."""
import re, sys
from collections import defaultdict

ARMS = ["shipped", "random", "whole", "evict", "evict-random", "evict-shipped", "evict-whole"]
ROW = re.compile(r"^(\w+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+(\w+)$")


def parse(path):
    kernel, step, rnd, arm, section = "?", None, None, None, None
    rows = defaultdict(list)  # (step, arm, section, metric) -> [(round, p50, p90, p99, max)]
    failed = []
    for line in open(path, errors="replace"):
        parts = line.rstrip("\n").split("\t", 2)
        text = parts[-1]
        text = re.sub(r"^\S+Z ", "", text)  # the runner's timestamp
        text = re.sub(r"\x1b\[[0-9;]*m", "", text).strip()
        if re.fullmatch(r"\d+\.\d+\.\d+-\d+-azure", text):
            kernel = text
        # An arm's header: the comparison's names its working sets `ws-ARM.log`, the
        # benches' `ws-bench-ARM.log`.
        m = re.match(r"^=== round (\d+), ([\w-]+): (.*)$", text)
        if m and m.group(2) in ARMS:
            rnd, arm = int(m.group(1)), m.group(2)
            step = "bench" if "ws-bench-" in m.group(3) else "compare"
            section = None
            continue
        m = re.match(r"^(firecracker(?:_restore|_density)?):", text)
        if m:
            section = m.group(1)
            continue
        if "FAILED" in text and text.startswith("---"):
            failed.append((rnd, text))
            continue
        m = ROW.match(text)
        if m and arm:
            metric = m.group(1)
            sec = section if step == "compare" else "bench"
            rows[(step, arm, sec, metric)].append((rnd, *map(float, m.group(2, 3, 4, 5))))
    return kernel, rows, failed


def p50s(entries):
    return ", ".join(f"{e[1]:g}" for e in sorted(entries))


for path in sys.argv[1:]:
    kernel, rows, failed = parse(path)
    print(f"## {path}: kernel {kernel}")
    print("| arm | memory file after a restore, MiB (p50 / max, each round) | FC | first beat p50, ms (shards / FC) | fleet PSS a VM, MiB (shards / FC) |")
    print("|---|---|---|---|---|")
    for arm in ARMS:
        mem = rows.get(("compare", arm, "firecracker_restore", "shards_rss_file"), [])
        fc = rows.get(("compare", arm, "firecracker_restore", "fc_rss_file"), [])
        beat = rows.get(("compare", arm, "firecracker_restore", "shards_to_beat"), [])
        fcb = rows.get(("compare", arm, "firecracker_restore", "fc_to_beat"), [])
        if not (mem or beat):
            continue
        fleet = rows.get(("compare", arm, "firecracker_density", "shards_fleet_pss"), [])
        fcfleet = rows.get(("compare", arm, "firecracker_density", "fc_fleet_pss"), [])
        memtxt = "; ".join(f"{e[1]:g} / {e[4]:g}" for e in sorted(mem))
        beattxt = "; ".join(f"{a[1] / 1000:.2f} / {b[1] / 1000:.2f}" for a, b in zip(sorted(beat), sorted(fcb)))
        fleettxt = "; ".join(f"{a[1]:g} / {b[1]:g}" for a, b in zip(sorted(fleet), sorted(fcfleet)))
        print(f"| {arm} | {memtxt} | {p50s(fc)} | {beattxt} | {fleettxt} |")
    print()
    print("| arm | cold restore p50, ms | warm request p50, ms | warm peak RSS, MiB | pooled run p50, ms |")
    print("|---|---|---|---|---|")
    for arm in ARMS:
        get = lambda metric: sorted(rows.get(("bench", arm, "bench", metric), []))
        cold, warm, rss, pooled = get("cold_restore"), get("warm_request"), get("warm_peak_rss"), get("run_template")
        if not (cold or pooled):
            continue
        ms = lambda xs: ", ".join(f"{e[1] / 1000:.2f}" for e in xs)
        print(f"| {arm} | {ms(cold)} | {ms(warm)} | {', '.join(f'{e[1]:g}' for e in rss)} | {ms(pooled)} |")
    for rnd, text in failed:
        print(f"round {rnd}: {text}")
    print()
