#!/usr/bin/env python3
"""What the guest kernel keeps of a microVM's memory on the path a run takes (M117 and
M123 measured cold boots alone): `shards run -m LIMIT`, through the daemon, its VM a
template of its size restored. For each VM size, the limit the daemon sizes to exactly it
(resources::memory_mib over the table this host's architecture uses, read from
crates/shards/src/resources.rs), run N times on each of B templates: each template a boot
of its own (the daemon stopped and the home's templates removed before it), made by its
first run, the rest restoring it. A template's runs keep what its boot left, so the boots
are the samples (M179: three runners' boots differed by up to 7.5 MiB at a size). Prints
each run, with the meminfo fields that say where the memory went, then per size: the
boots, runs, the overhead's p50, p90, p99 and max (the VM's KiB less MemAvailable), and
the least margin over the limit (MemAvailable less the limit), which a run needs at least
0 of.

    SHARDS_HOME=$(mktemp -d) measure-run.py target/release/shards [N] [B]
"""
import os, platform, re, shutil, statistics, subprocess, sys

shards = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 20
boots = int(sys.argv[3]) if len(sys.argv) > 3 else 1
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


def overhead_kib(mib):
    for size, kib in table:
        if size >= mib:
            return kib
    size, kib = table[-1]
    return kib + (mib - size) * slope


def memory_mib(limit_kib, default=256):
    even = lambda m: m + m % 2
    mib = even(max(-(-limit_kib // 1024), default))
    while mib * 1024 - overhead_kib(mib) < limit_kib:
        mib = even(max(-(-(limit_kib + overhead_kib(mib)) // 1024), mib + 2))
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
print("mib limit_kib boot run memavailable_kib overhead_kib margin_kib " + " ".join(FIELDS))


def new_boot():
    """The next run boots its template again: the daemon, which keeps warm VMs of the
    templates it has, stopped, and the home's templates gone."""
    subprocess.run([shards, "stop", "daemon"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(os.path.join(home, "templates"), ignore_errors=True)


summary = []
for mib in sizes:
    limit = mib * 1024 - overhead_kib(mib)
    if memory_mib(limit) != mib:
        print(f"{mib}: no limit the daemon sizes to it; skipped")
        continue
    overheads, margins = [], []
    for b in range(boots):
        if boots > 1:
            new_boot()
        for i in range(n):
            out = subprocess.run([shards, "run", "--rm", "-m", f"{limit}k", image, "cat", "/proc/meminfo"],
                                 capture_output=True, text=True, check=True).stdout
            info = {k: int(v) for k, v in re.findall(r"^(\w+):\s+(\d+) kB", out, re.M)}
            avail = info["MemAvailable"]
            overheads.append(mib * 1024 - avail)
            margins.append(avail - limit)
            print(mib, limit, b, i, avail, mib * 1024 - avail, avail - limit,
                  " ".join(str(info.get(f, "-")) for f in FIELDS), flush=True)
    summary.append((mib, overhead_kib(mib), overheads, min(margins)))
print("mib table_kib boots n p50 p90 p99 max least_margin")
for mib, kib, o, m in summary:
    print(mib, kib, boots, len(o), statistics.median(o), pct(o, 90), pct(o, 99), max(o), m)
