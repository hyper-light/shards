#!/usr/bin/env python3
"""What measure.py's samples say of each read (D188, D189): for each OUT/<case>.txt, the
read's span, the slice's peak memory.current, what its reclaim and faults counted
(memory.stat's pgscan, pgsteal, pgfault, pgmajfault, workingset_refault_file and
memory.events' high, as differences over the read), how long its tasks stalled on memory
(memory.pressure's some total), the CPU its VM's vCPU threads took (user and system ticks,
all and the busiest), and what KVM counted of its VM (debugfs, where sampled: page faults
taken, fixed and spurious, its 4 KiB and 2 MiB entries, exits, remote TLB flushes). Its
VM is the reader's, of those the slice holds (warm ones beside it): the one whose exits
grew most. KVM's counters are read by name: one of them (`mmu_rmaps_stat`) is a table.

    samples.py OUT
"""
import os, re, sys

STAT = ("pgscan", "pgsteal", "pgfault", "pgmajfault", "workingset_refault_file")
KVM = ("pf_taken", "pf_fixed", "pf_spurious", "pages_4k", "pages_2m", "exits", "halt_exits", "irq_exits",
       "mmio_exits", "remote_tlb_flush")


def parse(path):
    """Each sample of a read: its time, the slice's memory, its counters, its vCPUs'
    ticks and KVM's counters."""
    samples, cur = [], None
    with open(path) as f:
        for line in f:
            words = line.split()
            if not words:
                continue
            if words[0] == "==":
                cur = {"t": float(words[1]), "stat": {}, "vms": {}}
                pid = None
                samples.append(cur)
                if "current" in words:
                    cur["current"] = int(words[words.index("current") + 1])
            elif cur is None:
                continue
            elif words[0] == "events":
                pairs = dict(zip(words[1::2], words[2::2]))
                cur["high"] = int(pairs.get("high", 0))
            elif words[0] == "pressure":
                m = re.search(r"total=(\d+)", line)
                cur["stall_us"] = int(m.group(1)) if m else 0
            elif words[0] == "vm":
                # vm PID then name state utime stime for each vCPU, its KVM line after it.
                pid = words[1]
                rest = words[2:]
                ticks = {}
                for i in range(0, len(rest) - 3, 4):
                    ticks[rest[i]] = int(rest[i + 2]) + int(rest[i + 3])
                cur["vms"][pid] = {"vcpus": ticks, "kvm": {}}
            elif words[0] == "kvm" and pid in cur["vms"]:
                kvm = cur["vms"][pid]["kvm"]
                for i, k in enumerate(words[:-1]):
                    if k in KVM and words[i + 1].isdigit():
                        kvm[k] = int(words[i + 1])
            else:
                pairs = dict(zip(words[::2], words[1::2]))
                for k in STAT:
                    if pairs.get(k, "None").isdigit():
                        cur["stat"][k] = int(pairs[k])
    return [s for s in samples if "current" in s]


def delta(samples, get):
    """The last sample's value less the first's, of those that have it."""
    values = [v for v in (get(s) for s in samples) if v is not None]
    return values[-1] - values[0] if len(values) > 1 else None


def summary(name, samples):
    if not samples:
        return f"{name}: no samples"
    span = samples[-1]["t"] - samples[0]["t"]
    peak = max(s["current"] for s in samples) >> 20
    parts = [f"{name}: {span:.0f} s sampled, peak {peak} MiB"]
    for k in STAT:
        d = delta(samples, lambda s, k=k: s["stat"].get(k))
        if d is not None:
            parts.append(f"{k} +{d}")
    d = delta(samples, lambda s: s.get("high"))
    if d is not None:
        parts.append(f"high events +{d}")
    d = delta(samples, lambda s: s.get("stall_us"))
    if d is not None:
        parts.append(f"stalled {d / 1e6:.1f} s")
    # The reader's VM: of those the slice held, the one whose exits grew most.
    pids = set().union(*(s["vms"].keys() for s in samples))
    if pids:
        grew = lambda p: delta(samples, lambda s: s["vms"].get(p, {}).get("kvm", {}).get("exits")) or 0
        vm = max(pids, key=grew)
        of = lambda s: s["vms"].get(vm, {})
        names = set().union(*(of(s).get("vcpus", {}).keys() for s in samples))
        ticks = {n: delta(samples, lambda s, n=n: of(s).get("vcpus", {}).get(n)) or 0 for n in names}
        if ticks:
            busiest = max(ticks, key=ticks.get)
            parts.append(f"vm {vm} vCPU ticks {sum(ticks.values())} ({busiest} {ticks[busiest]})")
        for k in KVM:
            d = delta(samples, lambda s, k=k: of(s).get("kvm", {}).get(k))
            if d is not None:
                parts.append(f"kvm {k} +{d}")
    return ", ".join(parts)


out = sys.argv[1]
for name in sorted(os.listdir(out)):
    if name.endswith(".txt") and not name.endswith(".perf.txt") and not name.startswith("daemon-"):
        print(summary(name[:-4], parse(os.path.join(out, name))))
