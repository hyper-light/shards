#!/usr/bin/env python3
"""Where a just-started child spends CPU after /proc first shows it asleep (`S`).

Starts a child (`sleep 60`, or `cat` blocked reading a pipe nobody writes), reads its
/proc/<pid>/{stat,schedstat,syscall,wchan} in a tight loop for SECONDS, and prints every
change after the first `S`, with the time since spawn. Each line: t_us state wchan
syscall-nr run_ns wait_ns slices. Run beside load (LOAD spinners in this cgroup) as a
loaded host would be. Usage: probe.py sleep|pipe SECONDS LOAD [RUNS]
"""
import os, subprocess, sys, time

def read(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return "?"

def sample(pid):
    stat = read(f"/proc/{pid}/stat")
    state = stat.rsplit(")", 1)[-1].split()[0] if ")" in stat else "?"
    sched = read(f"/proc/{pid}/schedstat")
    sysc = read(f"/proc/{pid}/syscall").split()[0]
    return state, read(f"/proc/{pid}/wchan"), sysc, sched

def one(kind, seconds):
    if kind == "sleep":
        child = subprocess.Popen(["sleep", "60"])
        writer = None
    else:
        r, writer = os.pipe()
        child = subprocess.Popen(["cat"], stdin=r, stdout=subprocess.DEVNULL)
        os.close(r)
    t0 = time.monotonic()
    last, seen_s, out = None, False, []
    while time.monotonic() - t0 < seconds:
        s = sample(child.pid)
        if s != last:
            seen_s |= s[0] == "S"
            if seen_s:
                out.append((int((time.monotonic() - t0) * 1e6), s))
            last = s
    child.kill(); child.wait()
    if writer is not None:
        os.close(writer)
    return out

def main():
    kind, seconds, load = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
    runs = int(sys.argv[4]) if len(sys.argv) > 4 else 1
    spinners = [subprocess.Popen([sys.executable, "-c", "while True: pass"]) for _ in range(load)]
    try:
        for run in range(runs):
            out = one(kind, seconds)
            first = out[0][1][3].split() if out else ["0", "0", "0"]
            lastv = out[-1][1][3].split() if out else ["0", "0", "0"]
            grew = int(lastv[0]) - int(first[0])
            print(f"run {run} {kind}: after first S, run_ns grew {grew}, slices {first[2]}->{lastv[2]}, changes {len(out)}")
            for t, (st, wchan, sysc, sched) in out[:40]:
                print(f"  {t:>9} {st} {wchan:<28} sys={sysc:<4} {sched}")
    finally:
        for s in spinners:
            s.kill(); s.wait()

main()
