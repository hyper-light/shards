#!/usr/bin/env python3
"""What each vCPU costs a run (PM M183): `shards run --rm --cpus C IMAGE true`, timed by
its client from its start to its exit, for each count C, N times, the counts in turn after
each one's template is made; then, for each count, what the guest keeps of its memory (the
VM's KiB less MemAvailable and the free pages its CPUs' lists hold, as M179 counts it), its
CPUs as the guest sees them, and the VM process's resident memory while it runs. Prints
n, p50, p90, p99 and max in ms, with the host, its OS, its load and the revision.

    SHARDS_HOME=$(mktemp -d) run.py SHARDS [N] [COUNTS]
"""
import os, platform, re, statistics, subprocess, sys, time

shards = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 50
image = "alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6"
host_cpus = os.cpu_count() or 1
counts = [int(c) for c in sys.argv[3].split(",")] if len(sys.argv) > 3 else sorted({1, 2, 4, 8, host_cpus})


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def load():
    return os.getloadavg()


subprocess.run([shards, "pull", "-q", image], check=True, stdout=subprocess.DEVNULL)
print(f"host {platform.system()} {platform.machine()}, {host_cpus} CPUs; n {n} a count; counts {counts}")
print(f"load {load()}")
for c in counts:  # each count's template made first, untimed
    subprocess.run([shards, "run", "--rm", "--cpus", str(c), image, "true"], check=True)
times = {c: [] for c in counts}
for _ in range(n):
    for c in counts:
        began = time.monotonic()
        subprocess.run([shards, "run", "--rm", "--cpus", str(c), image, "true"], check=True)
        times[c].append((time.monotonic() - began) * 1000)
print("cpus n p50_ms p90_ms p99_ms max_ms")
for c in counts:
    t = times[c]
    print(c, len(t), f"{statistics.median(t):.2f}", f"{pct(t, 90):.2f}", f"{pct(t, 99):.2f}", f"{max(t):.2f}")
print(f"load {load()}")
print("cpus guest_cpus memtotal_kib memavailable_kib pcp_kib allocatable_kib less_than_one_kib vm_process_rss_kib")
first = None
for c in counts:
    out = subprocess.run([shards, "run", "--rm", "--cpus", str(c), image, "cat", "/proc/meminfo", "/proc/zoneinfo"],
                         capture_output=True, text=True, check=True).stdout
    info = {k: int(v) for k, v in re.findall(r"^(\w+):\s+(\d+) kB", out, re.M)}
    pcp = sum(int(x) for x in re.findall(r"^\s+count:\s+(\d+)$", out, re.M)) * 4
    guest_cpus = len(set(re.findall(r"^\s+cpu:\s+(\d+)$", out, re.M)))
    allocatable = info["MemAvailable"] + pcp
    first = allocatable if first is None else first
    # The VM process's resident memory while a run sleeps in it (inspect's State.Pid).
    name = f"vcpus-{c}"
    subprocess.run([shards, "run", "-d", "--name", name, "--cpus", str(c), image, "sleep", "30"],
                   check=True, stdout=subprocess.DEVNULL)
    time.sleep(1)
    pid = subprocess.run([shards, "inspect", "-f", "{{.State.Pid}}", name], capture_output=True, text=True).stdout.strip()
    rss = subprocess.run(["ps", "-o", "rss=", "-p", pid], capture_output=True, text=True).stdout.strip()
    subprocess.run([shards, "rm", "-f", name], stdout=subprocess.DEVNULL)
    print(c, guest_cpus, info.get("MemTotal", 0), info["MemAvailable"], pcp, allocatable, first - allocatable, rss or "-")
