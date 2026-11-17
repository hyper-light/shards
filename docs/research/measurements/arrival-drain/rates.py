#!/usr/bin/env python3
"""What each firecracker job log says of its host and its Arrival step (D209 Part 11 amended).

    rates.py JOB_LOG...   # GitHub's job logs (gh api repos/hyper-light/shards/actions/jobs/ID/logs)

For each log: the CPU model its Host step names (lscpu), the Arrival step's span from its
start to the next step's, the host's load its bench printed as it ended, and its rate_20 and
rate_40 rows: latency from when each request was due (p50 and max, s), requests sent,
failed, and done per second."""
import re, sys
from datetime import datetime


def stamp(line):
    m = re.match(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)\.\d+Z", line)
    return datetime.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S") if m else None


for path in sys.argv[1:]:
    lines = open(path, "rb").read().decode("utf-8", "replace").splitlines()
    host = next((l.split("Model name:")[1].strip() for l in lines if "Model name:" in l), "?")
    start = next((i for i, l in enumerate(lines) if "##[group]Run" in l and "--bench arrival" in l), None)
    if start is None:
        print(f"{path}: {host}: no Arrival step")
        continue
    end = next((i for i in range(start + 1, len(lines)) if "##[group]Run" in lines[i]), len(lines) - 1)
    began, ended = stamp(lines[start]), stamp(lines[end])
    span = int((ended - began).total_seconds()) if began and ended else None
    body = lines[start:end]
    load = next((l.split("load (1, 5, 15 min):")[1].strip() for l in body if "load (1, 5, 15 min):" in l), "?")
    rows = {}
    for l in body:
        m = re.search(r"\b(rate_(?:20|40)(?:_sent|_failed|_done)?)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)", l)
        if m:
            rows[m.group(1)] = [float(x) for x in m.group(2, 3, 4, 5)]
    latency = lambda k: f"p50 {rows[k][0] / 1e6:.2f} s, max {rows[k][3] / 1e6:.2f} s" if k in rows else "-"
    first = lambda k: f"{rows[k][0]:g}" if k in rows else "-"
    print(f"{path}: {host}; Arrival {span} s; load {load}; rate_20 {latency('rate_20')}; rate_40 "
          f"{latency('rate_40')}, sent {first('rate_40_sent')}, failed {first('rate_40_failed')}, "
          f"done {first('rate_40_done')}/s")
