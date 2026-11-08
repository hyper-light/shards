#!/usr/bin/env python3
"""Gives the vCPU threads of one home's VM processes a scheduling slice of their own on a
Linux host (EEVDF's custom slice: sched_setattr(2) with SCHED_OTHER and sched_runtime, Linux
6.12 and later, Documentation/scheduler/sched-eevdf.rst), from outside, while it runs: so
that phases.py can compare one build's starts with and without it, two homes of the same
build. Every --every ms it looks at each `shards-vm` process working in HOME (a VM process's
working directory is its home) and sets the slice of each of its threads named `vcpu*` and
`hold` (with --all, every thread) it has not set yet.

    slice.py HOME SLICE_US [--every MS] [--all]

Runs until killed; prints how many threads it set, as it ends."""
import argparse, ctypes, os, signal, struct, sys, time

p = argparse.ArgumentParser()
p.add_argument("home")
p.add_argument("slice_us", type=int)
p.add_argument("--every", type=float, default=5)
p.add_argument("--all", action="store_true")
a = p.parse_args()

libc = ctypes.CDLL(None, use_errno=True)
SYS_sched_setattr = {"x86_64": 314, "aarch64": 274}[os.uname().machine]
home = os.path.realpath(a.home)
# struct sched_attr, SCHED_ATTR_SIZE_VER0: size, policy, flags, nice, priority, runtime,
# deadline, period (include/uapi/linux/sched/types.h).
attr = struct.pack("=IIQiIQQQ", 48, 0, 0, 0, 0, a.slice_us * 1000, 0, 0)
buf = ctypes.create_string_buffer(attr, len(attr))
done, ours, failed = set(), {}, 0
stop = False
signal.signal(signal.SIGTERM, lambda *_: globals().__setitem__("stop", True))


def home_of(pid):
    # A VM process works in its home (its spawner's working directory, D185).
    # Only a yes is kept: a VM process's spawner forks it elsewhere, then it moves there.
    if pid not in ours:
        try:
            if os.readlink(f"/proc/{pid}/cwd") != home:
                return False
        except OSError:
            return False
        ours[pid] = True
    return True


while not stop:
    try:
        pids = [int(d) for d in os.listdir("/proc") if d.isdigit()]
    except OSError:
        pids = []
    for pid in pids:
        try:
            with open(f"/proc/{pid}/comm") as f:
                if not f.read().startswith("shards-vm"):
                    continue
        except OSError:
            continue
        if not home_of(pid):
            continue
        try:
            tids = os.listdir(f"/proc/{pid}/task")
        except OSError:
            continue
        for t in tids:
            tid = int(t)
            if tid in done:
                continue
            try:
                with open(f"/proc/{pid}/task/{t}/comm") as f:
                    name = f.read().strip()
            except OSError:
                continue
            if not a.all and not (name.startswith("vcpu") or name == "hold"):
                continue
            done.add(tid)
            if libc.syscall(SYS_sched_setattr, tid, buf, 0) != 0:
                failed += 1
                if failed <= 3:
                    print(f"sched_setattr {tid} ({name}): errno {ctypes.get_errno()}", file=sys.stderr)
    time.sleep(a.every / 1000)
print(f"slice.py: {len(done) - failed} threads given {a.slice_us} us, {failed} failed")
