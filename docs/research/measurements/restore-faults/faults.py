#!/usr/bin/env python3
"""Where a restore's time to its first beat goes, by host CPU: the Firecracker comparison's
restore arm (crates/shards/benches/firecracker.rs), which writes each restore's time to its
guest's first beat, shards' logged phases, and KVM's counters of the restored VM as its
guest first beat (`firecracker_restore-kvm.tsv`, beside the samples, SHARDS_BENCH_SAMPLES).

For each host CPU and each VMM: n, p50, p90, p99 and max of the time to the first beat, of
the memory file's pages the host mapped by then and of those it holds a private copy of
(what shards' recorder takes for a working set, read and written), and of each counter
(stage-2 faults taken, fixed and spurious, exits, MMIO exits, halts, port I/O), which are
read a few beats after the first; for shards' logged restores, the time from its vCPUs
ready to RESUMED and to its first beat, and those per stage-2 fault taken: what each fault
costs where the faults are the time.

Then the traced restores (`--traced N`, `firecracker_restore-trace/VMM-I.trace`): KVM's
trace of each, cut at its guest's first beat (its first `.` out of COM1) and, for shards',
at its RESUMED marker (its control page's MARKER register written 2), each from its vCPU's
first entry into the guest: how long the VMM took to get there from its spawn; how long
its vCPU spent in the guest (as KVM sees it: an exit its host's own hypervisor handles
without KVM is guest time), in KVM for each exit's reason, a stage-2 fault on its RAM
(`npf`) apart from one on MMIO, and in its VMM for those KVM hands it (`_vmm`); its stage-2
faults on RAM, KVM's time for each, and the guest time before each (where the faults come
back to back, what an exit and its return cost besides KVM's handling: each restore's
median and 10th percentile, and their sum, the rest of the guest's time apart); for
shards', from its RESUMED marker to its first beat apart; the beat's way out to the bench,
and its time to it from its spawn, as the bench saw it (beside the paired samples', what the
trace itself costs);
and the share of its faulted pages that the restore before it of the same VMM faulted too,
what a working set recorded from one restore would hold of the next.

    faults.py SAMPLES_DIR...   (one per run: a CI artifact each, or target/bench-samples)
"""
import collections, csv, glob, json, os, re, statistics, sys

COUNTERS = ("pf_taken", "pf_fixed", "pf_spurious", "exits", "mmio_exits", "halt_exits", "io_exits")


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def summary(xs, digits=1):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "none"
    return (f"n {len(xs):3} p50 {pct(xs, .5):10.{digits}f} p90 {pct(xs, .9):10.{digits}f} "
            f"p99 {pct(xs, .99):10.{digits}f} max {max(xs):10.{digits}f}")


def number(v):
    try:
        x = float(v)
    except ValueError:
        return None
    return None if x != x else x


# A trace line with each event's thread group (options/record-tgid), on a clock of ns
# printed in µs: `COMM-TID (TGID) [CPU] FLAGS SECS.USECS: EVENT: ARGS`.
TRACE_LINE = re.compile(r"^\s*(?P<comm>.*?)-(?P<tid>\d+)\s+\(\s*(?P<tgid>[\d-]+)\)\s+\[\d+\]\s+"
                        r"(?:\S+\s+)?(?P<s>\d+)\.(?P<us>\d{6}):\s+(?P<event>\w+):\s?(?P<args>.*)$")
# VMX's exit reasons by SVM's names (arch/x86/include/uapi/asm/{vmx,svm}.h).
REASONS = {"EPT_VIOLATION": "npf", "EPT_MISCONFIG": "npf_mmio", "IO_INSTRUCTION": "io", "HLT": "hlt",
           "MSR_READ": "msr", "MSR_WRITE": "msr", "MSR_WRITE_IMM": "msr",
           "EXTERNAL_INTERRUPT": "interrupt", "PREEMPTION_TIMER": "timer", "CPUID": "cpuid",
           "PAUSE_INSTRUCTION": "pause", "INTERRUPT_WINDOW": "vintr"}
# The 32-bit MMIO gap, whose first page is shards' control page: the guest's RAM lies
# below it (crates/abi CONTROL_PAGE, control::MARKER, marker::RESUMED).
MMIO_GAP, RESUMED = 0xC000_0000, 2
COM1, BEAT = 0x3F8, ord(".")
MMIO_WRITE = re.compile(r"mmio write len \d+ gpa 0x([0-9a-f]+) val 0x([0-9a-f]+)")
PIO_WRITE = re.compile(r"pio_write at 0x([0-9a-f]+) size \d+ count \d+ val 0x([0-9a-f]+)")


def account(events, start, cut):
    """Each vCPU's time from `start` to `cut` (ns) by where it was, the exits it took by
    reason, and its stage-2 faults on RAM, each with the guest time before it."""
    time, exits, faults = collections.Counter(), collections.Counter(), []
    state = {}  # tid: ["guest", since] or ["exit", reason, since, to_vmm, guest_before]

    def close(s, t):
        _, reason, since, to_vmm, _ = s
        time[reason] += (to_vmm if to_vmm is not None else t) - since
        if to_vmm is not None:
            time[reason + "_vmm"] += t - to_vmm
        exits[reason] += 1

    for t, tid, event, args in events:
        if t < start:
            continue
        if t > cut:
            break
        s = state.get(tid)
        if event == "kvm_entry":
            if s and s[0] == "exit":
                close(s, t)
            state[tid] = ["guest", t]
        elif event == "kvm_exit":
            reason = re.search(r"reason (\S+)", args)
            reason = reason[1] if reason else "?"
            before = None
            if s and s[0] == "guest":
                before = t - s[1]
                time["guest"] += before
            state[tid] = ["exit", REASONS.get(reason, reason), t, None, before]
        elif event == "kvm_page_fault" and s and s[0] == "exit":
            address = re.search(r"address 0x([0-9a-f]+)", args)
            if address and int(address[1], 16) < MMIO_GAP:
                s[1] = "npf"
                faults.append((int(address[1], 16) >> 12, s[4]))
            else:
                s[1] = "npf_mmio"
        elif event == "kvm_userspace_exit" and s and s[0] == "exit":
            s[3] = t
    for s in state.values():
        if s[0] == "guest":
            time["guest"] += cut - s[1]
        else:
            close(s, cut)
    return time, exits, faults


def traced(path):
    """One traced restore: its VMM, and what it spent where, or None for a trace that
    never reached its guest's first beat."""
    with open(path) as f:
        words = f.readline().split()
        head = dict(zip(words[1::2], words[2::2]))
        events = []
        for line in f:
            m = TRACE_LINE.match(line)
            if m and m["tgid"] == head["pid"]:
                events.append((int(m["s"]) * 10**9 + int(m["us"]) * 1000, m["tid"], m["event"], m["args"]))
    entries = [t for t, _, event, _ in events if event == "kvm_entry"]
    if not entries:
        return None
    first = entries[0]

    def written(pattern, where, value):
        for t, _, event, args in events:
            m = pattern.search(args) if t >= first else None
            if m and int(m[1], 16) == where and int(m[2], 16) == value:
                return t
        return None

    beat = written(PIO_WRITE, COM1, BEAT)
    if beat is None:
        return None
    resumed = written(MMIO_WRITE, MMIO_GAP, RESUMED)
    r = {"vmm": head["vmm"], "to_beat_us": (int(head["beat_ns"]) - int(head["spawned_ns"])) / 1e3,
         "setup_us": (first - int(head["spawned_ns"])) / 1e3, "beat_out_us": (int(head["beat_ns"]) - beat) / 1e3}
    for name, cut in (("beat", beat), ("resumed", resumed)):
        if cut is None:
            continue
        time, exits, faults = account(events, first, cut)
        r[name] = {"run_us": (cut - first) / 1e3, "time": time, "exits": exits,
                   "faults": faults, "pages": {page for page, _ in faults}}
    return r


by_host, traces = {}, {}
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
    # Each run's restores in order, so each is set beside the one before it.
    def order(path):
        return int(re.search(r"-(\d+)\.trace$", path)[1])
    for path in sorted(glob.glob(os.path.join(d, "firecracker_restore-trace", "*.trace")), key=order):
        r = traced(path)
        if r is None:
            print(f"{path}: no first beat traced")
            continue
        mine = traces.setdefault(host, {}).setdefault(r["vmm"], [])
        r["before"] = next((p for p in reversed(mine) if p["dir"] == d), None)
        r["dir"] = d
        mine.append(r)

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
        if vmm == "shards-logged":
            for end in ("resumed_us", "to_beat_us"):
                after = [(number(r[end]), number(r["vcpus_ready_us"]), number(r["pf_taken"])) for r in mine]
                span = [e - v for e, v, _ in after if e is not None and v is not None]
                each = [(e - v) / f for e, v, f in after if None not in (e, v, f) and f > 0]
                name = end[: -len("_us")]
                print(f"    {'ready_to_' + name + '_us':<18} {summary(span)}")
                print(f"    {'us_per_fault_' + name:<18} {summary(each)}")

def us(ns):
    return ns / 1e3


for host, vmms in sorted(traces.items()):
    print(f"== {host}: traced restores")
    for vmm, rs in sorted(vmms.items(), reverse=True):
        print(f"  {vmm}")
        for name, xs in (("to_beat_us", [r["to_beat_us"] for r in rs]),
                         ("setup_us", [r["setup_us"] for r in rs]),
                         ("beat_out_us", [r["beat_out_us"] for r in rs])):
            print(f"    {name:<22} {summary(xs)}")
        for cut in ("resumed", "beat"):
            mine = [r[cut] for r in rs if cut in r]
            if not mine:
                continue
            print(f"    to {cut}, from the vCPU's first entry")
            print(f"      {'run_us':<20} {summary([m['run_us'] for m in mine])}")
            print(f"      {'faults':<20} {summary([len(m['faults']) for m in mine])}")
            each = [us(m["time"]["npf"]) / len(m["faults"]) for m in mine if m["faults"]]
            print(f"      {'npf_us_each':<20} {summary(each)}")
            before = [[us(g) for _, g in m["faults"] if g is not None] for m in mine]
            print(f"      {'guest_before_npf_us':<20} {summary([statistics.median(b) for b in before if b])}"
                  "  (each restore's median)")
            print(f"      {'guest_before_npf_p10':<20} {summary([pct(b, .1) for b in before if b])}"
                  "  (each restore's 10th percentile)")
            print(f"      {'guest_to_npf_us':<20} {summary([sum(b) for b in before])}  (their sum)")
            print(f"      {'guest_rest_us':<20} "
                  f"{summary([us(m['time']['guest']) - sum(b) for m, b in zip(mine, before)])}")
            kinds = sorted({k for m in mine for k in m["time"]}, key=lambda k: -sum(m["time"][k] for m in mine))
            for k in kinds:
                print(f"      {k + '_us':<20} {summary([us(m['time'][k]) for m in mine])}")
            for k in sorted({k for m in mine for k in m["exits"]}):
                print(f"      {k + '_exits':<20} {summary([m['exits'][k] for m in mine])}")
            held = [len(r[cut]["pages"] & r["before"][cut]["pages"]) / len(r[cut]["pages"])
                    for r in rs if cut in r and r["before"] and cut in r["before"] and r[cut]["pages"]]
            print(f"      {'held_by_before':<20} {summary(held, 3)}")
        legs = [r for r in rs if "resumed" in r]
        if legs:
            print("    from RESUMED to the beat")
            print(f"      {'run_us':<20} {summary([r['beat']['run_us'] - r['resumed']['run_us'] for r in legs])}")
            print(f"      {'faults':<20} "
                  f"{summary([len(r['beat']['faults']) - len(r['resumed']['faults']) for r in legs])}")
            kinds = sorted({k for r in legs for k in r["beat"]["time"]},
                           key=lambda k: -sum(r["beat"]["time"][k] - r["resumed"]["time"][k] for r in legs))
            for k in kinds:
                print(f"      {k + '_us':<20} "
                      f"{summary([us(r['beat']['time'][k] - r['resumed']['time'][k]) for r in legs])}")
