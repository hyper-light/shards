#!/usr/bin/env python3
"""Where a restore's time to its first beat goes, by host CPU: the Firecracker comparison's
restore arm (crates/shards/benches/firecracker.rs), which writes each restore's time to its
guest's first beat, shards' logged phases, and KVM's counters of the restored VM as its
guest first beat (`firecracker_restore-kvm.tsv`, beside the samples, SHARDS_BENCH_SAMPLES).

For each host CPU and each VMM: n, p50, p90, p99 and max of the time to the first beat, of
the memory file's pages the host mapped by then and of those it holds a private copy of
(what shards' recorder takes for a working set, read and written), and of each counter
(stage-2 faults taken, fixed and spurious, exits, MMIO exits, halts, port I/O); the share
of the faults that mapped a page that a working set would hold, its reads and its private
copies (min(1, (touched + written) / pf_fixed)); for shards' logged restores, the time from
its vCPUs ready to RESUMED and to its first beat, and those per stage-2 fault taken: what
each fault costs where the faults are the time.

    faults.py SAMPLES_DIR...   (one per run: a CI artifact each, or target/bench-samples)
"""
import csv, glob, json, os, statistics, sys

COUNTERS = ("pf_taken", "pf_fixed", "pf_spurious", "exits", "mmio_exits", "halt_exits", "io_exits")


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def summary(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "none"
    return (f"n {len(xs):3} p50 {pct(xs, .5):10.1f} p90 {pct(xs, .9):10.1f} "
            f"p99 {pct(xs, .99):10.1f} max {max(xs):10.1f}")


def number(v):
    try:
        x = float(v)
    except ValueError:
        return None
    return None if x != x else x


by_host = {}
for d in sys.argv[1:]:
    tsv = os.path.join(d, "firecracker_restore-kvm.tsv")
    reports = glob.glob(os.path.join(d, "firecracker_restore-*.json"))
    if not os.path.exists(tsv) or not reports:
        print(f"{d}: no restore samples")
        continue
    with open(reports[0]) as f:
        host = json.load(f)["host"]
    rows = by_host.setdefault(host, [])
    with open(tsv) as f:
        rows.extend(csv.DictReader(f, delimiter="\t"))

for host, rows in sorted(by_host.items()):
    print(f"== {host}")
    for vmm in ("shards", "firecracker", "shards-logged"):
        mine = [r for r in rows if r["vmm"] == vmm]
        if not mine:
            continue
        print(f"  {vmm}")
        print(f"    {'to_beat_us':<18} {summary([number(r['to_beat_us']) for r in mine])}")
        for name in ("touched_pages", "written_pages") + COUNTERS:
            print(f"    {name:<18} {summary([number(r.get(name, '-')) for r in mine])}")
        held = []
        for r in mine:
            t, w, f = (number(r.get(k, "-")) for k in ("touched_pages", "written_pages", "pf_fixed"))
            if None not in (t, w, f) and f > 0:
                held.append(min(1.0, (t + w) / f))
        print(f"    {'ws_held_share':<18} {summary(held)}")
        if vmm == "shards-logged":
            for end in ("resumed_us", "to_beat_us"):
                after = [(number(r[end]), number(r["vcpus_ready_us"]), number(r["pf_taken"])) for r in mine]
                span = [e - v for e, v, _ in after if e is not None and v is not None]
                each = [(e - v) / f for e, v, f in after if None not in (e, v, f) and f > 0]
                name = end[: -len("_us")]
                print(f"    {'ready_to_' + name + '_us':<18} {summary(span)}")
                print(f"    {'us_per_fault_' + name:<18} {summary(each)}")
