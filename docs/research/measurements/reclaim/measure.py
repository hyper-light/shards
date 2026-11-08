#!/usr/bin/env python3
"""What a run's guest keeps of a file its workload read, what `memory.reclaim` frees of
it, and when the host has that memory back (the user's plan, item 4: reclaim on the host's
pressure, through the guest's cgroup, and free page reporting handing the pages back).

Each round is a privileged `shards run` of Alpine, no memory limit (its memory plugged as
it needs it, D148), with a volume holding a file of SIZE MiB. It reads the file, then its
cgroup (the run's: execs join it, D148) is asked to reclaim what memory.stat counts as its
inactive file pages, with swappiness=0, as init's RECLAIM builtin asks (D188). The VM
process's memory on the host (macOS: phys_footprint, proc_pid_rusage; Linux: VmRSS) is
read before the read, after it, and after the reclaim at 0.5, 1, 2, 4, 8, 16 and 32 s: in
alternate rounds reporting goes at its own pace (D151: a pass every 2 s, some 64 blocks of
each free list a pass), or is flushed at once after the reclaim (page_reporting.flush,
kernel patch 0005), as the guest's init does, whatever the reclaim's write returned: it
fails with EAGAIN where less was reclaimed than asked, which init takes as what there
was. Each write's exit status is kept (`said`).

    measure.py SHARDS ROUNDS SIZE_MIB   (SHARDS_HOME and DOCKER_CONFIG as the caller sets)

One JSON line a round on stdout, then each arm's n, p50, p90, p99 and max.
"""
import json, math, os, shutil, subprocess, sys, tempfile, time

ALPINE = ("mirror.gcr.io/library/alpine@sha256:"
          "5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8")
AFTER = [0.5, 1, 2, 4, 8, 16, 32]
MIB = 1 << 20


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


def pct(v, p):
    v = sorted(v)
    return v[min(max(1, math.ceil(p / 100 * len(v))), len(v)) - 1]


def main():
    shards, rounds, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    work = tempfile.mkdtemp(prefix="shards-reclaim-")
    rows = []
    try:
        with open(os.path.join(work, "big"), "wb") as f:
            block = os.urandom(MIB)
            for _ in range(size):
                f.write(block)
        for n in range(rounds):
            flush = n % 2 == 1
            name = f"reclaim-{os.getpid()}-{n}"
            run = subprocess.run([shards, "run", "-d", "--rm", "--privileged", "--name", name,
                                  "-v", f"{work}:/vol", ALPINE, "sleep", "3600"],
                                 capture_output=True, text=True)
            if run.returncode != 0:
                raise RuntimeError(f"run: {run.stderr.strip()}")
            try:
                pid = int(subprocess.run([shards, "inspect", "-f", "{{.State.Pid}}", name],
                                         capture_output=True, text=True, check=True).stdout)
                time.sleep(3)
                row = {"round": n, "flush": flush, "size_mib": size, "host": {}}
                row["host"]["before"] = host_bytes(pid)
                t0 = time.monotonic()
                sh(shards, name, "cat /vol/big >/dev/null")
                row["read_s"] = time.monotonic() - t0
                row["host"]["read"] = host_bytes(pid)
                before = stat(shards, name)
                row["guest_read"] = {k: before.get(k, 0) for k in
                                     ("file", "active_file", "inactive_file", "anon", "shmem",
                                      "MemFree")}
                cold = before.get("inactive_file", 0)
                flushing = ("; echo 1 >/sys/module/page_reporting/parameters/flush; echo flush $?"
                            if flush else "")
                t0 = time.monotonic()
                said = sh(shards, name, f"echo '{cold} swappiness=0' >/sys/fs/cgroup/memory.reclaim; "
                                        f"echo reclaim $?{flushing}")
                row["reclaim_s"] = time.monotonic() - t0
                row["said"] = said.split()
                start = time.monotonic()
                for at in AFTER:
                    time.sleep(max(0.0, start + at - time.monotonic()))
                    row["host"][f"{at}s"] = host_bytes(pid)
                after = stat(shards, name)
                row["guest_reclaimed"] = {k: after.get(k, 0) for k in
                                          ("file", "anon", "shmem", "MemFree")}
            finally:
                subprocess.run([shards, "kill", name], capture_output=True)
            print(json.dumps(row), flush=True)
            rows.append(row)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    for flush in (False, True):
        arm = [r for r in rows if r["flush"] == flush]
        if not arm:
            continue
        print(f"\n{'flushed' if flush else 'paced'} (n {len(arm)}), MiB")

        def line(label, v):
            print(f"  {label:28} p50 {pct(v, 50):8.1f} p90 {pct(v, 90):8.1f} "
                  f"p99 {pct(v, 99):8.1f} max {max(v):8.1f}")

        line("guest file after the read", [r["guest_read"]["file"] / MIB for r in arm])
        line("guest file after reclaim", [r["guest_reclaimed"]["file"] / MIB for r in arm])
        line("host: the read added", [(r["host"]["read"] - r["host"]["before"]) / MIB for r in arm])
        for at in AFTER:
            line(f"host: back after {at} s",
                 [(r["host"]["read"] - r["host"][f"{at}s"]) / MIB for r in arm])
        line("reclaim's exec (s)", [r["reclaim_s"] for r in arm])


if __name__ == "__main__":
    main()
