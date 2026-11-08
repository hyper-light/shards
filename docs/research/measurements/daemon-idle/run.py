#!/usr/bin/env python3
"""What a daemon and its runs cost while they idle (review findings 7.9, 7.16, 7.22): with
N running containers, each a microVM whose command sleeps, over SECONDS: the daemon's CPU
time, idle wakeups (top's IDLEW: wakeups of an idle CPU on the process's behalf, macOS),
threads and RSS; and the same of the VMs' processes together. The counts go up in turn,
containers added to reach each; a count of 0 measures the daemon alone.

    run.py BIN_DIR IMAGE COUNTS SECONDS SCRATCH_DIR

BIN_DIR holds `shards`, `shardsd`, `shards-vm` and `shards-net`, signed as scripts/hvf-run
signs them (target/e2e after an E2E run). COUNTS ascend, comma-separated: 0,10,100.
"""
import os
import platform
import re
import shutil
import subprocess
import sys
import time


def shards(bin_dir, home, *args, check=True):
    env = dict(os.environ, SHARDS_HOME=home)
    out = subprocess.run([os.path.join(bin_dir, "shards"), *args], env=env,
                         capture_output=True, text=True)
    if check and out.returncode != 0:
        raise SystemExit(f"shards {' '.join(args)}: {out.returncode}: {out.stderr}")
    return out.stdout


def processes(home):
    """The daemon's pid, and its VMs' (their command lines name the home)."""
    daemon = int(open(os.path.join(home, "daemon.pid")).read().strip())
    ps = subprocess.run(["ps", "-axo", "pid=,args="], capture_output=True, text=True).stdout
    vms = [int(l.split(None, 1)[0]) for l in ps.splitlines()
           if "shards-vm" in l and home in l]
    return daemon, vms


def seconds(cputime):
    """`ps`'s cputime ([[dd-]hh:]mm:ss.ss) in seconds."""
    days, _, rest = cputime.rpartition("-")
    parts = [float(p) for p in rest.split(":")]
    total = 0.0
    for p in parts:
        total = total * 60 + p
    return total + (int(days) * 86400 if days else 0)


def usage(pids):
    """Each pid's CPU seconds, threads and RSS (KiB), of those still there."""
    if not pids:
        return {}
    out = subprocess.run(["ps", "-o", "pid=,cputime=,rss=", "-p", ",".join(map(str, pids))],
                         capture_output=True, text=True).stdout
    got = {}
    for line in out.splitlines():
        pid, cpu, rss = line.split()
        got[int(pid)] = (seconds(cpu), int(rss))
    return got


def threads(pid):
    out = subprocess.run(["ps", "-M", "-p", str(pid)], capture_output=True, text=True).stdout
    return max(0, len(out.splitlines()) - 1)


def wakeups(pids, window):
    """Idle wakeups over `window` seconds, of each pid together (top's second sample)."""
    args = ["top", "-l", "2", "-s", str(window), "-n", str(len(pids)), "-stats", "pid,idlew"]
    for pid in pids:
        args += ["-pid", str(pid)]
    out = subprocess.run(args, capture_output=True, text=True).stdout
    samples = out.split("PID")
    total = 0
    for line in samples[-1].splitlines()[1:]:
        m = re.match(r"\s*(\d+)\s+(\d+)", line)
        if m and int(m.group(1)) in pids:
            total += int(m.group(2))
    return total


def main():
    bin_dir, image, counts, window, scratch = sys.argv[1:6]
    counts = [int(c) for c in counts.split(",")]
    window = int(window)
    home = os.path.join(scratch, "idle-home")
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home, mode=0o700)
    shards(bin_dir, home, "pull", "-q", image)
    rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True,
                         text=True).stdout.strip()
    print(f"host {platform.node()} {platform.machine()} {platform.platform()}, rev {rev}, "
          f"window {window} s")
    running = 0
    for n in counts:
        while running < n:
            shards(bin_dir, home, "run", "-d", "--name", f"idle-{running}", image,
                   "sleep", "100000000")
            running += 1
        deadline = time.time() + 120
        while len(shards(bin_dir, home, "ps", "-q").split()) < n:
            if time.time() > deadline:
                raise SystemExit(f"{n} containers did not all run")
            time.sleep(0.5)
        time.sleep(5)
        daemon, vms = processes(home)
        # CPU over the daemon's window of wakeups; the VMs' wakeups over a second one.
        before = usage([daemon, *vms])
        daemon_wakeups = wakeups([daemon], window)
        after = usage([daemon, *vms])
        vm_wakeups = wakeups(vms, window) if vms else 0
        d_cpu = after[daemon][0] - before[daemon][0]
        v_cpu = sum(after[p][0] - before[p][0] for p in vms if p in before and p in after)
        v_rss = sum(after[p][1] for p in vms if p in after)
        print(f"{n:5} containers: daemon cpu {d_cpu:.3f} s/{window} s, idle wakeups "
              f"{daemon_wakeups}/{window} s, threads {threads(daemon)}, rss "
              f"{after[daemon][1] / 1024:.1f} MiB; {len(vms)} VM processes: cpu "
              f"{v_cpu:.3f} s/{window} s, idle wakeups {vm_wakeups}/{window} s, rss "
              f"{v_rss / 1024:.1f} MiB")
    ids = shards(bin_dir, home, "ps", "-aq").split()
    if ids:
        shards(bin_dir, home, "rm", "-f", *ids, check=False)
    shards(bin_dir, home, "daemon", "stop", check=False)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
