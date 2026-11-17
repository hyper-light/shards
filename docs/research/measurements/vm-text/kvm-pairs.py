#!/usr/bin/env python3
"""Each VMM's restores in a run of the Firecracker comparison, snapshot pair by pair: the
medians of its `firecracker_restore-kvm.tsv` columns (benches/firecracker.rs), its first beat,
the guest pages touched and written, the faults taken and the exits by kind.

    kvm-pairs.py firecracker_restore-kvm.tsv"""
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
cols = ["to_beat_us", "touched_pages", "written_pages", "pf_taken", "exits", "mmio_exits", "halt_exits", "io_exits"]
by = {}
for r in rows:
    by.setdefault((r["vmm"], r["snapshot"]), []).append(r)
print("vmm          snap  n  " + "  ".join(f"{c[:13]:>13}" for c in cols))
for (vmm, snap), rs in sorted(by.items(), key=lambda kv: (kv[0][1], kv[0][0])):
    med = lambda c: statistics.median(float(r[c]) for r in rs if r[c] not in ("", "NaN"))
    print(f"{vmm:12} {snap:>4} {len(rs):2}  " + "  ".join(f"{med(c):13.1f}" for c in cols))
