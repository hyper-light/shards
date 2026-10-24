#!/usr/bin/env python3
"""Every PERIOD seconds, each thread of each process named NAME: its scheduling policy as
thread_info reports it (1 timeshare, 2 the real-time or fixed modes), its current and base
priority, and its CPU time, from proc_pidinfo (PROC_PIDLISTTHREADS, PROC_PIDTHREADINFO).
One line a thread a look: seconds since the start, pid, thread name, policy, curpri,
priority, CPU ns. XNU demotes a time-constraint thread that ran 100 quanta without
blocking to timeshare for twice that (osfmk/kern/priority.c, the fail-safe); this sees it.

    threads.py NAME SECONDS [PERIOD]   (macOS)

It runs at the time-constraint policy itself, as the vCPUs do, so that the threads it
watches keeping the CPUs do not keep it from looking.
"""
import ctypes, ctypes.util, subprocess, sys, time

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
libc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
PROC_PIDLISTTHREADS, PROC_PIDTHREADINFO = 6, 5


class ThreadInfo(ctypes.Structure):
    _fields_ = [("user", ctypes.c_uint64), ("system", ctypes.c_uint64), ("cpu_usage", ctypes.c_int32),
                ("policy", ctypes.c_int32), ("run_state", ctypes.c_int32), ("flags", ctypes.c_int32),
                ("sleep_time", ctypes.c_int32), ("curpri", ctypes.c_int32), ("priority", ctypes.c_int32),
                ("maxpriority", ctypes.c_int32), ("name", ctypes.c_char * 64)]


def threads(pid):
    ids = (ctypes.c_uint64 * 512)()
    n = libc.proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, ids, ctypes.sizeof(ids))
    for handle in ids[: max(n, 0) // 8]:
        info = ThreadInfo()
        if libc.proc_pidinfo(pid, PROC_PIDTHREADINFO, handle, ctypes.byref(info), ctypes.sizeof(info)) == ctypes.sizeof(info):
            yield info


class TimeConstraint(ctypes.Structure):
    _fields_ = [("period", ctypes.c_uint32), ("computation", ctypes.c_uint32),
                ("constraint", ctypes.c_uint32), ("preemptible", ctypes.c_int)]


def realtime():
    """THREAD_TIME_CONSTRAINT_POLICY (2) on this thread: 2 ms of computation in 5 ms, the
    tick's ticks converted by mach_timebase_info."""
    base = (ctypes.c_uint32 * 2)()
    libc.mach_timebase_info(base)
    ticks = lambda ns: ns * base[1] // base[0]
    policy = TimeConstraint(0, ticks(2_000_000), ticks(5_000_000), 1)
    libc.pthread_mach_thread_np.restype = ctypes.c_uint32
    me = libc.pthread_mach_thread_np(libc.pthread_self())
    if libc.thread_policy_set(me, 2, ctypes.byref(policy), 4) != 0:
        print("# time-constraint policy refused", file=sys.stderr)


def main():
    libc.pthread_self.restype = ctypes.c_void_p
    libc.pthread_mach_thread_np.argtypes = [ctypes.c_void_p]
    realtime()
    name, seconds = sys.argv[1], float(sys.argv[2])
    period = float(sys.argv[3]) if len(sys.argv) > 3 else 0.02
    start = time.monotonic()
    pids, pids_at = [], -1.0
    while (now := time.monotonic() - start) < seconds:
        if now - pids_at > 0.5:
            out = subprocess.run(["pgrep", "-x", name], capture_output=True, text=True).stdout
            pids, pids_at = [int(p) for p in out.split()], now
        lines = []
        for pid in pids:
            at = time.monotonic() - start
            for t in threads(pid):
                lines.append(f"{at:.4f} {pid} {t.name.decode(errors='replace') or '-'} {t.policy} {t.curpri} "
                             f"{t.priority} {t.user + t.system}")
        if lines:
            print("\n".join(lines), flush=True)
        time.sleep(max(0.0, period - (time.monotonic() - start - now)))


main()
