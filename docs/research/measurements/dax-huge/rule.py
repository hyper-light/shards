#!/usr/bin/env python3
"""measure.py's decision rule (its docstring's, fixed before any number was taken) applied
to its outputs, one a host. For each host: each cell's tables (for each kind and arm, its
rounds and the reads or writes timed, and of the per-round values the median and range of
p50 and p99, the median of p90 and the largest max; seq's seconds; write's bytes written
back per write; what the slice, the page cache, the VM's mappings and KVM held), then
whether huge and huge-random pass against window, each comparison that fails named. Then
for each class of host (its CPU's vendor, whether a hypervisor is beneath it, its volume's
file system) the arm it takes.

    rule.py OUT.txt...
"""
import re
import statistics
import sys
from collections import defaultdict

LATENCY = {"touch": ("first", "second"), "write": ("write",), "budget-touch": ("first", "second")}
ARMS = ("huge", "huge-random")


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def read(path):
    host = {"path": path, "vendor": "-", "hypervisor": "-", "fs": "-", "cells": defaultdict(list),
            "failed": defaultdict(int), "rounds": 0, "head": []}
    for line in open(path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("host "):
            host["head"].append(line)
            m = re.search(r"; vendor (\S+); hypervisor (\S+);", line)
            if m:
                host["vendor"], host["hypervisor"] = m.group(1), m.group(2)
        elif line.startswith(("thp ", "arms ")):
            host["head"].append(line)
            m = re.search(r"; rounds (\d+);", line)
            if m:
                host["rounds"] = int(m.group(1))
        elif line.startswith("volume "):
            host["head"].append(line)
            m = re.search(r": (\S+) on ", line)
            if m:
                host["fs"] = m.group(1)
        elif line.startswith("cell "):
            fields = dict(w.split("=", 1) for w in line.split()[1:] if "=" in w)
            host["cells"][(fields["kind"], fields["arm"])].append(fields)
        elif line.startswith("FAILED "):
            m = re.search(r"arm=(\S+) kind=(\S+)", line)
            if m:
                host["failed"][(m.group(2), m.group(1))] += 1
    return host


def values(host, kind, arm, key):
    return [v for v in (num(c.get(key)) for c in host["cells"].get((kind, arm), [])) if v is not None]


def spread(v):
    return f"{statistics.median(v):.1f} [{min(v):.1f}, {max(v):.1f}]" if v else "-"


def tables(host):
    print("kind arm: rounds; per read or write, n, p50 median [range], p90 median, p99 median [range], max (µs); "
          "or seconds median [range]")
    kinds = sorted({k for k, _ in host["cells"]}, key=lambda k: ("seq-sparse", "seq-written", "touch", "write",
                                                               "budget-seq", "budget-touch").index(k))
    for kind in kinds:
        for arm in ("window", "huge", "huge-random", "fuse"):
            cells = host["cells"].get((kind, arm))
            if not cells:
                continue
            head = f"  {kind:12} {arm:11}: {len(cells)}"
            if kind in LATENCY:
                for what in LATENCY[kind]:
                    n = int(sum(values(host, kind, arm, f"{what}_n")))
                    p90 = values(host, kind, arm, f"{what}_p90")
                    print(f"{head}; {what} n {n}, p50 {spread(values(host, kind, arm, f'{what}_p50'))}, p90 "
                          f"{statistics.median(p90):.1f}, p99 {spread(values(host, kind, arm, f'{what}_p99'))}, "
                          f"max {max(values(host, kind, arm, f'{what}_max') or [0]):.1f}" if p90 else f"{head}; {what} -")
            else:
                print(f"{head}; s {spread(values(host, kind, arm, 's'))}")
            held = {k: values(host, kind, arm, k) for k in
                    ("written_per_write", "peak_file", "peak_file_thp", "peak_dirty", "held", "pmd_kib", "maps", "hg", "rr",
                     "kvm_pf_taken", "kvm_pages_2m", "kvm_pages_4k", "high", "oom_kill")}
            if any(held.values()):
                print("      " + "; ".join(f"{k} {statistics.median(v):.0f}" for k, v in held.items() if v))


def verdict(host, arm):
    """The comparisons arm `arm` fails against window on this host: none where it passes."""
    fails = []

    def worse(kind, key):
        mine, base = values(host, kind, arm, key), values(host, kind, "window", key)
        if not mine or not base:
            fails.append(f"{kind} {key}: missing")
            return None
        if min(mine) > max(base):
            fails.append(f"{kind} {key}: [{min(mine):.1f}, {max(mine):.1f}] above window's [{min(base):.1f}, {max(base):.1f}]")
            return True
        return False

    def better(kind, key):
        mine, base = values(host, kind, arm, key), values(host, kind, "window", key)
        return bool(mine and base and max(mine) < min(base))

    # 1. seq: neither file worse, one better.
    seq = [worse(k, "s") for k in ("seq-sparse", "seq-written")]
    if not any(seq) and None not in seq and not (better("seq-sparse", "s") or better("seq-written", "s")):
        fails.append("seq: neither file faster than window's beyond the spread")
    # 2. touch and write: p50 and p99.
    for kind, whats in (("touch", ("first", "second")), ("write", ("write",))):
        for what in whats:
            for q in ("p50", "p99"):
                worse(kind, f"{what}_{q}")
    # 3. bytes written back per write.
    worse("write", "written_per_write")
    # 4. budget: every run finished, none killed; seq's seconds and touch's first reads.
    for kind in ("budget-seq", "budget-touch"):
        ran, failed = len(host["cells"].get((kind, arm), [])), host["failed"].get((kind, arm), 0)
        if failed or ran < host["rounds"]:
            fails.append(f"{kind}: {ran} of {host['rounds']} runs finished")
        if sum(values(host, kind, arm, "oom_kill")) > 0:
            fails.append(f"{kind}: runs killed (oom_kill {sum(values(host, kind, arm, 'oom_kill')):.0f})")
    worse("budget-seq", "s")
    worse("budget-touch", "first_p50")
    worse("budget-touch", "first_p99")
    return fails


def main(paths):
    classes = defaultdict(list)
    for path in paths:
        host = read(path)
        print(f"== {path}")
        for line in host["head"]:
            print(line)
        tables(host)
        passes = {}
        for arm in ARMS:
            if not any(a == arm for _, a in host["cells"]):
                continue
            fails = verdict(host, arm)
            passes[arm] = not fails
            print(f"  {arm}: {'passes' if not fails else 'fails: ' + '; '.join(fails)}")
        kind = (f"{host['vendor']} {'virtual machine' if host['hypervisor'] == 'yes' else 'bare metal'} "
                f"on {host['fs']}")
        seq = {arm: values(host, "seq-written", arm, "s") for arm in ARMS}
        classes[kind].append((path, passes, seq))
    print("== classes")
    for kind, hosts in classes.items():
        takes = [arm for arm in ARMS if all(passes.get(arm) for _, passes, _ in hosts)]
        if len(takes) > 1:
            takes.sort(key=lambda arm: statistics.median([s for _, _, seq in hosts for s in seq[arm]]))
        print(f"  {kind} ({len(hosts)} hosts): takes {takes[0] if takes else 'none'}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    main(sys.argv[1:])
