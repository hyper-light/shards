#!/usr/bin/env python3
"""Summarize retained native proxy wait samples, without inventing missing metrics."""
import json
import math
import pathlib
import sys

for proxy_path in map(pathlib.Path, sys.argv[1:]):
    proxy_rows = [
        json.loads(line.split("PROXY_CAPACITY_SAMPLE ", 1)[1])
        for line in proxy_path.read_text().splitlines()
        if "PROXY_CAPACITY_SAMPLE " in line
    ]
    proxy_summary = {"file": str(proxy_path), "n": len(proxy_rows)}
    for proxy_key in ["voluntary_switches", "cpu_ticks", "cpu_ns", "refill_ns", "stop_ns", "minor_faults", "major_faults", "fds_live", "sample_ns"]:
        proxy_values = sorted(row[proxy_key] for row in proxy_rows if row.get(proxy_key) is not None)
        if proxy_values:
            proxy_summary[proxy_key] = {
                str(percent): proxy_values[max(0, math.ceil(len(proxy_values) * percent / 100) - 1)]
                for percent in [50, 90, 99, 100]
            }
    print(json.dumps(proxy_summary, sort_keys=True))
