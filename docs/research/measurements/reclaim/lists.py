#!/usr/bin/env python3
"""Where a reclaimed file's memory is between the guest's reclaim and the host having it
back (PM M267 on Linux: back in steps over 8 s, flushed or not, where macOS had it within
0.5 s). One privileged `shards run` of Alpine with a volume holding a file of SIZE MiB:
it reads the file, its cgroup reclaims its inactive file pages (`memory.reclaim` with
swappiness=0) and flushes page reporting (page_reporting.flush, kernel patch 0005), as
init's RECLAIM builtin does (D188); then, at 0, 1, 2, 4, 8 and 16 s, the VM process's
memory on the host (measure.py's `host_bytes`) and, in the guest, its per-CPU page
lists' pages and highest `high` (/proc/zoneinfo), its free lists by order
(/proc/buddyinfo), MemFree, the flush parameter's presence and what page reporting has
reported (/proc/vmstat), and its CPUs.

    lists.py SHARDS SIZE_MIB ROUNDS   (SHARDS_HOME and DOCKER_CONFIG as the caller sets)

One line a reading."""
import json, os, re, shutil, subprocess, sys, tempfile, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from measure import ALPINE, MIB, host_bytes, sh  # noqa: E402


def guest(shards, name):
    out = sh(shards, name, "cat /proc/zoneinfo; echo ===; cat /proc/buddyinfo; echo ===; "
             "grep -E '^(MemFree|MemTotal):' /proc/meminfo; echo ===; nproc; "
             "ls /sys/module/page_reporting/parameters/; echo ===; "
             "grep -E '^(file|anon) ' /sys/fs/cgroup/memory.stat")
    zone, buddy, mem, rest, cg = out.split("===")
    return {
        "pcp_pages": sum(int(m) for m in re.findall(r"^\s+count:\s+(\d+)", zone, re.M)),
        "pcp_high_max": max([int(m) for m in re.findall(r"^\s+high:\s+(\d+)", zone, re.M)] or [0]),
        "buddy": [l.split(None, 4)[-1] for l in buddy.strip().splitlines()],
        "mem": mem.split(),
        "cpus_and_params": rest.split(),
        "cgroup": cg.split(),
    }


def main():
    shards, size, rounds = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    work = tempfile.mkdtemp(prefix="shards-lists-")
    try:
        with open(os.path.join(work, "big"), "wb") as f:
            block = os.urandom(MIB)
            for _ in range(size):
                f.write(block)
        for n in range(rounds):
            name = f"lists-{os.getpid()}-{n}"
            run = subprocess.run([shards, "run", "-d", "--rm", "--privileged", "--name", name,
                                  "-v", f"{work}:/vol", ALPINE, "sleep", "3600"],
                                 capture_output=True, text=True)
            if run.returncode != 0:
                raise RuntimeError(f"run: {run.stderr.strip()}")
            try:
                pid = int(subprocess.run([shards, "inspect", "-f", "{{.State.Pid}}", name],
                                         capture_output=True, text=True, check=True).stdout)
                time.sleep(3)
                say = lambda at: print(json.dumps({"round": n, "at": at,
                                                   "host_mib": round(host_bytes(pid) / MIB, 1),
                                                   "guest": guest(shards, name)}), flush=True)
                say("before")
                sh(shards, name, "cat /vol/big >/dev/null")
                say("read")
                t0 = time.monotonic()
                said = sh(shards, name,
                          "echo \"$(grep '^inactive_file ' /sys/fs/cgroup/memory.stat | cut -d' ' -f2) "
                          "swappiness=0\" > /sys/fs/cgroup/memory.reclaim; echo reclaim $?; "
                          "echo 1 > /sys/module/page_reporting/parameters/flush; echo flush $?")
                print(json.dumps({"round": n, "reclaim_and_flush_s": round(time.monotonic() - t0, 3),
                                  "said": said.split()}), flush=True)
                for at in (0, 1, 2, 4, 8, 16):
                    time.sleep(max(0.0, t0 + at - time.monotonic()))
                    say(f"{at}s")
            finally:
                subprocess.run([shards, "kill", name], capture_output=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
