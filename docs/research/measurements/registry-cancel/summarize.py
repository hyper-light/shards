#!/usr/bin/env python3
"""Nearest-rank sample distributions; values remain measurements, never policy."""
import json, math, pathlib, re, sys
root = pathlib.Path(sys.argv[1])
summary = []
for path in sorted(root.rglob('*.log')):
    rows = []
    for line in path.read_text(errors='replace').splitlines():
        match = re.search(r'REGISTRY_CANCEL mode=(\w+)(?: cancelling=(\w+))? sample=(\d+) cancel_ns=(\d+) voluntary_switches=(\d+) thread_cpu_us=(\d+)', line)
        if match:
            mode, cancelling, sample, ns, switches, cpu = match.groups()
            rows.append(dict(mode=mode, cancelling=cancelling, sample=int(sample), latency_ns=int(ns), voluntary_switches=int(switches), thread_cpu_us=int(cpu)))
    if not rows: continue
    dist = {}
    for field in ('latency_ns', 'voluntary_switches', 'thread_cpu_us'):
        values = sorted(row[field] for row in rows)
        dist[field] = {name: values[max(0,math.ceil(p*len(values))-1)] for name,p in [('p50',.5),('p90',.9),('p99',.99),('max',1)]}
    summary.append(dict(path=str(path.relative_to(root)), n=len(rows), samples=rows, distributions=dist))
print(json.dumps(summary, indent=2))
