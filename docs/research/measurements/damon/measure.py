#!/usr/bin/env python3
"""DAMON's reclaim against D188's, in a run's guest (the user's plan, item 4: reclaim on
the host's pressure, "measure DAMON as the alternative"). The pinned kernel has
DAMON_RECLAIM built in (Firecracker's base configs, both architectures): a kernel thread,
kdamond, that samples the guest's physical memory's accesses every 5 ms and pages out
what has gone unaccessed for `min_age` (120 s), as its quotas allow (10 ms of CPU and
128 MiB a second), while the guest's free memory is between its watermarks (mm/damon/
reclaim.c, core.c `damos_wmark_wait_us`, Linux 6.18).

Each round is a privileged `shards run` of Alpine, no memory limit (its memory plugged as
it needs it, D148), with a volume holding a file of SIZE MiB, which it reads. Then, by
arm:

  none           nothing: what the guest keeps, and what it costs idle
  d188           what init's RECLAIM builtin does at the host's pressure (D188): its
                 cgroup's inactive file pages to memory.reclaim with swappiness=0, then
                 page_reporting.flush (kernel patch 0005)
  damon-default  damon_reclaim enabled, its parameters as the kernel has them
  damon-active   damon_reclaim enabled, active whatever the guest's free memory
                 (wmarks_high and wmarks_mid 1000, wmarks_low 0)
  damon-fast     damon-active, min_age 5 s

The VM process's memory on the host (macOS: phys_footprint; Linux: VmRSS) is read before
the read, after it, and every 5 s after the arm begins, for the arm's window (40 s for
none and d188, 90 for damon-fast, 180 for the others); then its CPU time on the host
(Linux: /proc/PID/stat; macOS: ps's), and kdamond's in the guest, over 30 s of idleness;
then the file is read again, timed: what reclaim costs a workload that comes back for it.
The guest's memory.stat, damon_reclaim's own counts and the region it chose
(monitor_region_start and _end, set to the biggest System RAM when unset) and the guest's
/proc/iomem are kept. Arms rotate their order from round to round.

    measure.py SHARDS ROUNDS SIZE_MIB [ARM...]   (SHARDS_HOME and DOCKER_CONFIG as the
                                                 caller sets)

SHARDS is a build with `no-pressure.patch`, run with SHARDS_EXPERIMENT_NO_PRESSURE set
(which this sets for it): the daemon's own reclaim at the host's pressure (D188) would
otherwise take the cold file pages any arm measures, as it did once on a host whose
service cgroup stalled. The daemon's log is read at the end, and a pass of it said.

One JSON line a round on stdout, then each arm's n, p50, p90, p99 and max.
"""
import json, math, os, platform, shutil, subprocess, sys, tempfile, time

ALPINE = ("mirror.gcr.io/library/alpine@sha256:"
          "5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8")
MIB = 1 << 20
STEP = 5
IDLE = 30
WINDOW = {"none": 40, "d188": 40, "damon-default": 180, "damon-active": 180,
          "damon-fast": 90}
DAMON = "/sys/module/damon_reclaim/parameters"
SETTINGS = {
    "damon-default": {},
    "damon-active": {"wmarks_high": 1000, "wmarks_mid": 1000, "wmarks_low": 0},
    "damon-fast": {"wmarks_high": 1000, "wmarks_mid": 1000, "wmarks_low": 0,
                   "min_age": 5000000},
}
# /proc/PID/stat's times are in USER_HZ ticks, 100 a second on x86 and arm64
# (include/asm-generic/param.h, which both include).
USER_HZ = 100


def host_bytes(pid):
    """The VM process's memory on the host."""
    if sys.platform == "darwin":
        import ctypes
        lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib")

        class Info(ctypes.Structure):  # struct rusage_info_v0
            _fields_ = [("uuid", ctypes.c_uint8 * 16), ("user", ctypes.c_uint64),
                        ("system", ctypes.c_uint64), ("idle_wkups", ctypes.c_uint64),
                        ("interrupt_wkups", ctypes.c_uint64), ("pageins", ctypes.c_uint64),
                        ("wired", ctypes.c_uint64), ("resident", ctypes.c_uint64),
                        ("phys_footprint", ctypes.c_uint64),
                        ("proc_start_abstime", ctypes.c_uint64),
                        ("proc_exit_abstime", ctypes.c_uint64)]
        info = Info()
        if lib.proc_pid_rusage(pid, 0, ctypes.byref(info)) != 0:
            raise OSError(f"proc_pid_rusage({pid})")
        return info.phys_footprint
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) * 1024
    raise OSError(f"no VmRSS for {pid}")


def host_cpu(pid):
    """The VM process's CPU time on the host, user and system, in seconds."""
    if sys.platform == "darwin":
        out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True,
                             text=True, check=True).stdout.strip()
        seconds = 0.0
        for part in out.split(":"):
            seconds = seconds * 60 + float(part)
        return seconds
    fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def sh(shards, name, script):
    r = subprocess.run([shards, "exec", name, "sh", "-c", script], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"exec {script!r}: {r.returncode}: {r.stderr.strip()}")
    return r.stdout


def stat(shards, name):
    out = sh(shards, name, "cat /sys/fs/cgroup/memory.stat; grep -E '^(MemTotal|MemFree):' /proc/meminfo")
    got = {}
    for line in out.splitlines():
        k, _, v = line.partition(" ")
        if k.endswith(":"):
            got[k[:-1]] = int(v.split()[0]) * 1024
        elif v.strip().isdigit():
            got[k] = int(v)
    return got


def damon(shards, name):
    """damon_reclaim's parameters and counts, and kdamond's CPU time, in seconds."""
    out = sh(shards, name, f"cd {DAMON} && for p in *; do echo \"$p $(cat $p)\"; done")
    got = dict(line.split(" ", 1) for line in out.splitlines() if " " in line)
    pid = int(got.get("kdamond_pid", "-1"))
    if pid > 0:
        fields = sh(shards, name, f"cat /proc/{pid}/stat").rsplit(")", 1)[1].split()
        got["kdamond_cpu_s"] = (int(fields[11]) + int(fields[12])) / USER_HZ
    return got


def pct(v, p):
    v = sorted(v)
    return v[min(max(1, math.ceil(p / 100 * len(v))), len(v)) - 1]


def one(shards, work, arm, n):
    name = f"damon-{os.getpid()}-{n}"
    # The microVM's own PID namespace, where kdamond, a kernel thread, is seen.
    run = subprocess.run([shards, "run", "-d", "--rm", "--privileged", "--pid", "host",
                          "--name", name, "-v", f"{work}:/vol", ALPINE, "sleep", "3600"],
                         capture_output=True, text=True)
    if run.returncode != 0:
        raise RuntimeError(f"run: {run.stderr.strip()}")
    try:
        pid = int(subprocess.run([shards, "inspect", "-f", "{{.State.Pid}}", name],
                                 capture_output=True, text=True, check=True).stdout)
        time.sleep(3)
        row = {"round": n, "arm": arm, "host": {}}
        row["host"]["before"] = host_bytes(pid)
        t0 = time.monotonic()
        sh(shards, name, "cat /vol/big >/dev/null")
        row["read_s"] = time.monotonic() - t0
        row["host"]["read"] = host_bytes(pid)
        read = stat(shards, name)
        row["guest_read"] = {k: read.get(k, 0) for k in
                             ("file", "active_file", "inactive_file", "anon", "MemFree",
                              "MemTotal")}
        start = time.monotonic()
        if arm == "d188":
            sh(shards, name, f"echo '{read.get('inactive_file', 0)} swappiness=0' "
                             ">/sys/fs/cgroup/memory.reclaim || true; "
                             "echo 1 >/sys/module/page_reporting/parameters/flush")
        elif arm in SETTINGS:
            writes = "".join(f"echo {v} >{DAMON}/{k} && " for k, v in SETTINGS[arm].items())
            sh(shards, name, f"{writes}echo Y >{DAMON}/enabled")
        row["arm_s"] = time.monotonic() - start
        for at in range(STEP, WINDOW[arm] + 1, STEP):
            time.sleep(max(0.0, start + at - time.monotonic()))
            row["host"][f"{at}s"] = host_bytes(pid)
        row["guest_after"] = {k: v for k, v in stat(shards, name).items() if k in
                              ("file", "active_file", "inactive_file", "anon", "MemFree",
                               "MemTotal")}
        if arm in SETTINGS:
            row["damon_after"] = damon(shards, name)
            row["iomem"] = sh(shards, name, "cat /proc/iomem")
        cpu0, guest0 = host_cpu(pid), row.get("damon_after", {}).get("kdamond_cpu_s")
        time.sleep(IDLE)
        row["idle_cpu_pct"] = (host_cpu(pid) - cpu0) / IDLE * 100
        if guest0 is not None:
            row["kdamond_idle_cpu_pct"] = (damon(shards, name)["kdamond_cpu_s"] - guest0) / IDLE * 100
        row["host"]["idle"] = host_bytes(pid)
        t0 = time.monotonic()
        sh(shards, name, "cat /vol/big >/dev/null")
        row["reread_s"] = time.monotonic() - t0
        row["host"]["reread"] = host_bytes(pid)
        return row
    finally:
        subprocess.run([shards, "kill", name], capture_output=True)


def main():
    shards, rounds, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    os.environ["SHARDS_EXPERIMENT_NO_PRESSURE"] = "1"
    arms = sys.argv[4:] or list(WINDOW)
    work = tempfile.mkdtemp(prefix="shards-damon-")
    rows = []
    try:
        with open(os.path.join(work, "big"), "wb") as f:
            block = os.urandom(MIB)
            for _ in range(size):
                f.write(block)
        for n in range(rounds):
            for arm in arms[n % len(arms):] + arms[:n % len(arms)]:
                row = one(shards, work, arm, n)
                print(json.dumps(row), flush=True)
                rows.append(row)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    log = os.path.join(os.environ.get("SHARDS_HOME", ""), "daemon.log")
    passes = [l for l in open(log, errors="replace") if "short of memory" in l] if os.path.exists(log) else []
    watched = os.path.exists(log) and any("pressure is not watched: an experiment" in l
                                          for l in open(log, errors="replace"))
    print(f"\nhost {platform.node()}, {platform.system()} {platform.release()} "
          f"{platform.machine()}; {size} MiB read; MiB, s or % of a CPU; the daemon's "
          f"pressure watcher {'off' if watched else 'NOT off'}, {len(passes)} passes of it")

    def line(label, v):
        print(f"  {label:34} n {len(v):3} p50 {pct(v, 50):8.1f} p90 {pct(v, 90):8.1f} "
              f"p99 {pct(v, 99):8.1f} max {max(v):8.1f}")

    for arm in arms:
        got = [r for r in rows if r["arm"] == arm]
        if not got:
            continue
        print(f"\n{arm}")
        line("guest file after the read", [r["guest_read"]["file"] / MIB for r in got])
        line("guest file at the window's end", [r["guest_after"]["file"] / MIB for r in got])
        line("host: the read added", [(r["host"]["read"] - r["host"]["before"]) / MIB for r in got])
        for at in (at for at in (10, 20, 30, 60, 90, 120, 150, 180) if at <= WINDOW[arm]):
            line(f"host: back after {at} s",
                 [(r["host"]["read"] - r["host"][f"{at}s"]) / MIB for r in got])
        line("VM process idle CPU (%)", [r["idle_cpu_pct"] for r in got])
        if all("kdamond_idle_cpu_pct" in r for r in got):
            line("kdamond idle CPU (%)", [r["kdamond_idle_cpu_pct"] for r in got])
            line("damon reclaimed (MiB)",
                 [int(r["damon_after"].get("bytes_reclaimed_regions", 0)) / MIB for r in got])
        line("re-read (s)", [r["reread_s"] for r in got])


if __name__ == "__main__":
    main()
