#!/usr/bin/env python3
"""What the guest kernel keeps of a microVM's memory on the path a run takes (M117 and
M123 measured cold boots alone): `shards run -m LIMIT`, through the daemon, its VM a
template of its size restored. For each VM size, the limit the daemon sizes to exactly it
(resources::memory_mib over the table this host's architecture uses, read from
crates/shards/src/resources.rs), run N times on each of B templates: each template a boot
of its own (the daemon stopped and the home's templates removed before it), made by its
first run, the rest restoring it. A template's runs keep what its boot left, so the boots
are the samples. What the guest keeps is the VM's KiB less what may be allocated: its
MemAvailable and the free pages its CPUs' lists hold, which MemFree leaves out until they
go back to the buddy allocator (mm/page_alloc.c, account_freepages) and an allocation
takes first, or drains (M179: those held 4 to 22 MiB, and the boots of a size differed by
up to 15.4 MiB in MemAvailable, by 0.7 MiB with them). Prints each run, with the
meminfo fields that say where the memory went, then per size: the boots, runs, what the
guest keeps at p50, p90, p99 and max, and the least margin over the limit (what may be
allocated, less the limit), which a run needs at least 0 of.

    SHARDS_HOME=$(mktemp -d) measure-run.py target/release/shards [N] [B] [CPUS]

CPUS, a comma-separated list of vCPU counts (`--cpus`), measures each size at each (M183:
each vCPU keeps memory of its own); then it prints, per size, what each vCPU past the
first keeps, and per count the most the vCPUs past the first keep together, from which
the table's VCPUS: the second's, and each's past it.
"""
import os, platform, re, shutil, statistics, subprocess, sys

shards = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 20
boots = int(sys.argv[3]) if len(sys.argv) > 3 else 1
counts = [int(c) for c in sys.argv[4].split(",")] if len(sys.argv) > 4 else [1]
home = os.environ["SHARDS_HOME"]
# Where a boot's memory went: the fields of /proc/meminfo that differ between boots.
FIELDS = ("MemTotal", "MemFree", "Cached", "Shmem", "SReclaimable", "SUnreclaim", "KernelStack",
          "PageTables", "Percpu", "AnonPages", "AnonHugePages", "Mapped")
image = "alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6"
here = os.path.dirname(os.path.abspath(__file__))
source = open(os.path.join(here, "..", "..", "..", "..", "crates", "shards", "src", "resources.rs")).read()

x86 = platform.machine() in ("x86_64", "AMD64")
# The table for this architecture: the first MEASURED after `#[cfg(target_arch = "x86_64")]`,
# or the one after `#[cfg(not(target_arch = "x86_64"))]`.
marker = '#[cfg(target_arch = "x86_64")]' if x86 else '#[cfg(not(target_arch = "x86_64"))]'
at = source.index(marker + "\n    const MEASURED")
table = [(int(s.replace("_", "")), int(k.replace("_", "")))
         for s, k in re.findall(r"\((\d[\d_]*), (\d[\d_]*)\)", source[at:source.index("];", at)])]
slope = int(re.search(re.escape(marker) + r"\n    const SLOPE: u64 = (\d+);", source).group(1))
vcpus_found = re.search(re.escape(marker) + r"\n    const VCPUS: \[u64; 2\] = \[([\d_]+), ([\d_]+)\];", source)
second, each = (int(vcpus_found.group(i).replace("_", "")) for i in (1, 2)) if vcpus_found else (0, 0)


def overhead_kib(mib, cpus=1):
    extra = 0 if cpus < 2 else second + (cpus - 2) * each
    for size, kib in table:
        if size >= mib:
            return kib + extra
    size, kib = table[-1]
    return kib + (mib - size) * slope + extra


def memory_mib(limit_kib, cpus=1, default=256):
    even = lambda m: m + m % 2
    mib = even(max(-(-limit_kib // 1024), default))
    while mib * 1024 - overhead_kib(mib, cpus) < limit_kib:
        mib = even(max(-(-(limit_kib + overhead_kib(mib, cpus)) // 1024), mib + 2))
    return mib


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def host_mib():
    if platform.system() == "Darwin":
        return int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout) >> 20
    return int(re.search(r"^MemTotal:\s+(\d+) kB", open("/proc/meminfo").read(), re.M).group(1)) >> 10


subprocess.run([shards, "pull", "-q", image], check=True, stdout=subprocess.DEVNULL)
# Each table size and the midpoint of each interval it bounds (memory_mib takes the
# overhead of the next size up), as far as half the host's memory, which runs beside the
# rest of the host as they do.
most = host_mib() // 2
sizes = sorted(s for s in {s for s, _ in table} | {(a + b) // 2 // 2 * 2 for (a, _), (b, _) in zip(table, table[1:])}
               if s <= most)
print(f"host {platform.system()} {platform.machine()}, {host_mib()} MiB; n {n} a boot, {boots} boots a size, "
      f"sizes to {most} MiB")
print("mib limit_kib boot run memavailable_kib kept_kib margin_kib " + " ".join(FIELDS) + " pcp_kib cpus")


def new_boot():
    """The next run boots its template again: the daemon, which keeps warm VMs of the
    templates it has, stopped, and the home's templates gone."""
    subprocess.run([shards, "stop", "daemon"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(os.path.join(home, "templates"), ignore_errors=True)


summary = []
for mib in sizes:
    for c in counts:
        limit = mib * 1024 - overhead_kib(mib, c)
        if memory_mib(limit, c) != mib:
            print(f"{mib}: no limit the daemon sizes to it with {c} vCPUs; skipped")
            continue
        overheads, margins = [], []
        for b in range(boots):
            if boots > 1:
                new_boot()
            for i in range(n):
                out = subprocess.run([shards, "run", "--rm", "-m", f"{limit}k", "--cpus", str(c), image, "cat",
                                      "/proc/meminfo", "/proc/zoneinfo"],
                                     capture_output=True, text=True, check=True).stdout
                info = {k: int(v) for k, v in re.findall(r"^(\w+):\s+(\d+) kB", out, re.M)}
                avail = info["MemAvailable"]
                # The free pages the CPUs' lists hold (/proc/zoneinfo's pagesets, 4 KiB pages
                # on both architectures' guest kernels), and how many CPUs hold them.
                pcp = sum(int(c) for c in re.findall(r"^\s+count:\s+(\d+)$", out, re.M)) * 4
                cpus = len(set(re.findall(r"^\s+cpu:\s+(\d+)$", out, re.M)))
                kept = mib * 1024 - avail - pcp
                overheads.append(kept)
                margins.append(avail + pcp - limit)
                print(mib, limit, b, i, avail, kept, avail + pcp - limit,
                      " ".join(str(info.get(f, "-")) for f in FIELDS), pcp, cpus, flush=True)
        summary.append((mib, c, overhead_kib(mib, c), overheads, min(margins)))
print("mib cpus table_kib boots n p50 p90 p99 max least_margin")
for mib, c, kib, o, m in summary:
    print(mib, c, kib, boots, len(o), statistics.median(o), pct(o, 90), pct(o, 99), max(o), m)
# What each vCPU past the first keeps, at each size: the most kept with c vCPUs less the
# most with one, over c - 1 (M183); the table takes the most of these.
most = {(mib, c): max(o) for mib, c, _, o, _ in summary}
slopes = [((most[(mib, c)] - most[(mib, 1)]) / (c - 1), mib, c)
          for (mib, c) in most if c > 1 and (mib, 1) in most]
if slopes:
    print("mib cpus kib_a_vcpu")
    for kib, mib, c in sorted(slopes, key=lambda x: (x[1], x[2])):
        print(mib, c, f"{kib:.0f}")
    extra = {c: max(most[(mib, c)] - most[(mib, 1)] for (mib, k) in most if k == c and (mib, 1) in most)
             for c in sorted({c for (_, c) in most if c > 1})}
    print("cpus most_kept_past_one")
    for c, kib in extra.items():
        print(c, kib)
    if 2 in extra:
        past = [(kib - extra[2]) / (c - 2) for c, kib in extra.items() if c > 2]
        print("vcpus second", extra[2], "each", f"{max(past):.0f}" if past else "-")
