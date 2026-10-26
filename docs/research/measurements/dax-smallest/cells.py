#!/usr/bin/env python3
"""Sustained I/O's npm and cargo cells on a volume (sustained-io/run.py's) at DAX_SMALLEST
thresholds (D189; the plan's steps 2 and 3), on macOS and Linux. One build of shards
carrying smallest.patch, one home holding shards-sio:1 (sustained-io/images.sh on Linux,
images-mac.sh on a Mac); each arm a threshold, SHARDS_DAX_SMALLEST bytes, under the host's
default DAX window. Each run is `shards run --rm --cpus 8 -m 8192m --memory-swap 8192m -v
RUN/vol:/vol shards-sio:1 /sio/work CELL volume`, its tree cloned fresh into RUN/vol from
SIO/ctx (a clone or a reflink where the filesystem has them), the daemon stopped as the
arm changes, so that its share processes take the next arm's threshold, and an unrecorded
run made first of each arm's daemon (its first start, template and pool); arms and cells
interleaved, their order turning each round. Per run: the host's wall time (total), the
workload's own (work.sh's work_ns) and its cgroup's memory.peak in the guest, the VM
process's peak memory (the largest of the home's daemon's shards-vm descendants seen while
it ran, sampled every 250 ms: macOS proc_pid_rusage's ri_lifetime_max_phys_footprint, Linux
/proc/PID/status VmHWM; the run's VM is the largest, a pool's idle VMs hold little), the
daemon log's DAX-END lines (the run's VM's windows, ranges, mappings, removals, host
mappings, descriptors, memory), and the host's load. A run that fails is said, with its
last lines, and counted.

    cells.py SHARDS --home HOME --sio SIO [--arms 2097152,1048576] [--cells npm,cargo]
             [--rounds 5] [--out OUT] [--rev REV]

The header's revision is REV where given, else that of the checkout SHARDS was built in
(SHARDS being its target/release/shards), with whether it carries changes.
"""
import argparse, ctypes, ctypes.util, os, platform, shutil, statistics, subprocess, sys, tempfile, threading, time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("--home", required=True)
p.add_argument("--sio", required=True)
p.add_argument("--arms", default="2097152,1048576")
p.add_argument("--cells", default="npm,cargo")
p.add_argument("--rounds", type=int, default=5)
p.add_argument("--out", default="")
p.add_argument("--rev", default="")
a = p.parse_args()

ARMS = [int(x) for x in a.arms.split(",")]
CELLS = a.cells.split(",")
TREES = {"npm": "cli", "cargo": "rust-analyzer"}
home = Path(a.home).resolve()
sio = Path(a.sio).resolve()
config = Path(tempfile.mkdtemp(prefix="sio-mac-"))
runs = Path(tempfile.mkdtemp(prefix="sio-runs-", dir=sio))
out = open(a.out, "a") if a.out else None

macos = sys.platform == "darwin"
libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
RUSAGE_INFO_V4 = 4


class RusageV4(ctypes.Structure):
    # struct rusage_info_v4 (sys/resource.h), as far as ri_lifetime_max_phys_footprint and on.
    _fields_ = [("ri_uuid", ctypes.c_uint8 * 16)] + [(n, ctypes.c_uint64) for n in (
        "ri_user_time", "ri_system_time", "ri_pkg_idle_wkups", "ri_interrupt_wkups", "ri_pageins",
        "ri_wired_size", "ri_resident_size", "ri_phys_footprint", "ri_proc_start_abstime",
        "ri_proc_exit_abstime", "ri_child_user_time", "ri_child_system_time",
        "ri_child_pkg_idle_wkups", "ri_child_interrupt_wkups", "ri_child_pageins",
        "ri_child_elapsed_abstime", "ri_diskio_bytesread", "ri_diskio_byteswritten",
        "ri_cpu_time_qos_default", "ri_cpu_time_qos_maintenance", "ri_cpu_time_qos_background",
        "ri_cpu_time_qos_utility", "ri_cpu_time_qos_legacy", "ri_cpu_time_qos_user_initiated",
        "ri_cpu_time_qos_user_interactive", "ri_billed_system_time", "ri_serviced_system_time",
        "ri_logical_writes", "ri_lifetime_max_phys_footprint")] + [("rest", ctypes.c_uint64 * 32)]


def peak_memory(pid):
    """The process's peak memory in bytes, None where it has gone."""
    if macos:
        info = RusageV4()
        if libc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(info)) != 0:
            return None
        return info.ri_lifetime_max_phys_footprint
    try:
        for l in Path(f"/proc/{pid}/status").read_text().splitlines():
            if l.startswith("VmHWM:"):
                return int(l.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        return None
    return None


def say(line):
    print(line, flush=True)
    if out:
        out.write(line + "\n")
        out.flush()


def env(arm):
    e = dict(os.environ, SHARDS_HOME=str(home), DOCKER_CONFIG=str(config), SHARDS_DAX_SMALLEST=str(arm))
    e.pop("SHARDS_SHARE_WINDOW_MIB", None)
    return e


def stop(arm):
    subprocess.run([a.shards, "stop", "daemon"], env=env(arm), capture_output=True)


def vm_pids():
    """The VM processes of this home's daemon (daemon.pid), its descendants alone: other
    sessions' daemons run VMs on this host too."""
    try:
        root = int((home / "daemon.pid").read_text().split()[0])
    except (OSError, ValueError, IndexError):
        return []
    table = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,comm="], capture_output=True, text=True).stdout
    kids, comm = {}, {}
    for l in table.splitlines():
        w = l.split(None, 2)
        if len(w) == 3:
            pid, ppid = int(w[0]), int(w[1])
            kids.setdefault(ppid, []).append(pid)
            comm[pid] = w[2]
    found, todo = [], [root]
    while todo:
        pid = todo.pop()
        for k in kids.get(pid, []):
            todo.append(k)
            if comm.get(k, "").endswith("/shards-vm") or comm.get(k) == "shards-vm":
                found.append(k)
    return found


def run(arm, cell, n):
    r = runs / f"r{n}"
    vol = r / "vol"
    vol.mkdir(parents=True)
    clone = ["cp", "-c", "-R"] if macos else ["cp", "-a", "--reflink=auto"]
    subprocess.run(clone + [str(sio / "ctx" / TREES[cell]), str(vol / TREES[cell])], check=True)
    log = home / "daemon.log"
    before = log.stat().st_size if log.exists() else 0
    peaks = {}
    done = threading.Event()

    def watch():
        while not done.is_set():
            for pid in vm_pids():
                f = peak_memory(pid)
                if f is not None:
                    peaks[pid] = max(peaks.get(pid, 0), f)
            done.wait(0.25)

    w = threading.Thread(target=watch, daemon=True)
    load = os.getloadavg()[0]
    t = time.monotonic()
    w.start()
    proc = subprocess.run([a.shards, "run", "--rm", "--pull", "never", "--cpus", "8", "-m", "8192m",
                           "--memory-swap", "8192m", "-v", f"{vol}:/vol", "shards-sio:1",
                           "/sio/work", cell, "volume"], env=env(arm), capture_output=True, text=True)
    total = time.monotonic() - t
    done.set()
    w.join()
    said = []
    if log.exists():
        with open(log, "rb") as f:
            f.seek(before)
            said = [l.decode(errors="replace").rstrip() for l in f]
    ends = [l.split("DAX-END ", 1)[1] for l in said if "DAX-END " in l]
    shutil.rmtree(r, ignore_errors=True)
    work = guest_peak = None
    for l in proc.stdout.splitlines():
        if l.startswith("SIO "):
            for kv in l.split()[1:]:
                k, _, v = kv.partition("=")
                if k == "work_ns":
                    work = int(v) / 1e9
                elif k == "guest_peak":
                    guest_peak = int(v) / (1 << 20)
    if proc.returncode != 0 or work is None:
        say(f"FAILED {arm} {cell}: status {proc.returncode}")
        for l in (proc.stdout + proc.stderr).splitlines()[-40:]:
            say(f"  | {l}")
        return None
    peak = max(peaks.values()) if peaks else 0
    return {"total": total, "work": work, "peak_mib": peak / (1 << 20), "guest_peak_mib": guest_peak or 0,
            "load": load, "ends": ends}


def warm_up(arm):
    """An unrecorded run as each arm's daemon starts: its first start, template and pool,
    which the recorded runs do not pay again."""
    r = runs / "warm"
    (r / "vol").mkdir(parents=True, exist_ok=True)
    got = subprocess.run([a.shards, "run", "--rm", "--pull", "never", "--cpus", "8", "-m", "8192m",
                          "--memory-swap", "8192m", "-v", f"{r / 'vol'}:/vol", "shards-sio:1", "true"],
                         env=env(arm), capture_output=True, text=True)
    shutil.rmtree(r, ignore_errors=True)
    if got.returncode != 0:
        say(f"warm-up of arm {arm} failed: status {got.returncode}: {(got.stdout + got.stderr).strip()[-400:]}")


def host_header():
    # The build's tree: target/release/shards's checkout, its head and whether it carries changes
    # (smallest.patch).
    tree = Path(a.shards).resolve().parents[2]
    rev = a.rev or subprocess.run(["git", "-C", str(tree), "rev-parse", "--short", "HEAD"],
                                  capture_output=True, text=True).stdout.strip() or "-"
    dirty = not a.rev and subprocess.run(["git", "-C", str(tree), "diff", "--quiet"],
                                         capture_output=True).returncode != 0
    if macos:
        cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
        osname = "macOS " + platform.mac_ver()[0]
    else:
        cpu = next((l.split(":", 1)[1].strip() for l in Path("/proc/cpuinfo").read_text().splitlines()
                    if l.startswith("model name")), "-")
        osname = platform.freedesktop_os_release().get("PRETTY_NAME", "Linux")
    affinity = "" if macos else f"; CPUs {sorted(os.sched_getaffinity(0))}"
    return (f"host {platform.node()}; {osname}; kernel {platform.release()}; {platform.machine()}; {cpu}; "
            f"shards {rev}{' + changes (smallest.patch)' if dirty else ''} at {a.shards}{affinity}")


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))]


say(host_header())
say(f"arms (SHARDS_DAX_SMALLEST) {ARMS}; cells {CELLS} on a volume; {a.rounds} rounds")
got = {}
failed = []
n = 0
current = None
# The home's daemon, of whatever settings it was started with, stopped first: each arm's
# starts with that arm's.
stop(ARMS[0])
try:
    pairs = [(arm, cell) for arm in ARMS for cell in CELLS]
    for rnd in range(a.rounds):
        k = rnd % len(pairs)
        order = pairs[k:] + pairs[:k]
        if rnd % 2:
            order = order[::-1]
        for arm, cell in order:
            if current != arm:
                if current is not None:
                    stop(current)
                current = arm
                warm_up(arm)
            n += 1
            row = run(arm, cell, n)
            if row is None:
                failed.append((rnd, arm, cell))
                continue
            got.setdefault((arm, cell), []).append(row)
            end = " | ".join(row["ends"]) or "-"
            say(f"round {rnd} arm {arm} {cell} load {row['load']:.2f} total {row['total']:.2f}s "
                f"work {row['work']:.2f}s vm_peak {row['peak_mib']:.0f} MiB guest_peak {row['guest_peak_mib']:.0f} MiB; {end}")
finally:
    if current is not None:
        stop(current)
    shutil.rmtree(config, ignore_errors=True)
    shutil.rmtree(runs, ignore_errors=True)
say(f"load {os.getloadavg()[0]:.2f} at the end; runs failed {len(failed)}: {failed or '-'}")
for (arm, cell), rows in sorted(got.items()):
    for k in ("total", "work", "peak_mib", "guest_peak_mib"):
        v = [r[k] for r in rows]
        say(f"{cell:5} arm {arm:>8} {k:14} n {len(v)}: p50 {statistics.median(v):.2f} p90 {q(v, .9):.2f} "
            f"p99 {q(v, .99):.2f} max {max(v):.2f} min {min(v):.2f}")
