# Whether a pressure record read under a jiffy after the read before shows its stalls (D215,
# R-D's owed R09; CI 44282442232): python3 -I poll.py TICKS [CPU [READER_CPU]], on Linux.
# Two spinners pinned to CPU (default: this process's first allowed CPU) wait on each other.
# Over TICKS ticks of the coarse clock in each arm, the reader, pinned to READER_CPU (default:
# another allowed CPU, so that nothing takes its CPU, as the hung test had its own), reads
# /proc/pressure/cpu in a tight loop ("tight"), then once a tick ("tick"). Each arm says its
# reads, the record's `some` total's growth, and the spinners' own wait for their CPU
# (schedstat `run_delay`, summed) over the arm's wall time. The tick is the resolution of
# CLOCK_MONOTONIC_COARSE (clock_getres(2)), a jiffy, the unit in which PSI weights each CPU's
# stall time (kernel/sched/psi.c `collect_percpu_times`).
import os, signal, sys, time

if sys.platform != "linux":
    raise SystemExit(f"poll.py reads Linux's /proc/pressure and schedstat: not on {sys.platform}")

# Linux's CLOCK_MONOTONIC_COARSE, 6 (include/uapi/linux/time.h), which Python's time module
# does not name; its clock_gettime and clock_getres take any clock ID.
COARSE = 6


def some_total():
    with open("/proc/pressure/cpu") as f:
        for line in f:
            if line.startswith("some "):
                return int(line.split("total=")[1])
    raise SystemExit("no `some` line in /proc/pressure/cpu")


def run_delay(pid):
    with open(f"/proc/{pid}/schedstat") as f:
        return int(f.read().split()[1])


def spinner(cpu):
    pid = os.fork()
    if pid == 0:
        os.sched_setaffinity(0, {cpu})
        while True:
            pass
    return pid


def arm(name, spinners, ticks, tick_ns, tight):
    start = time.clock_gettime_ns(COARSE)
    end = start + ticks * tick_ns
    total, waited, began = some_total(), sum(map(run_delay, spinners)), time.monotonic_ns()
    reads = 1
    last = start
    while True:
        now = time.clock_gettime_ns(COARSE)
        if now >= end:
            break
        if tight or now > last:
            some_total()
            reads += 1
            last = now
    grew, ran = some_total() - total, sum(map(run_delay, spinners)) - waited
    wall = time.monotonic_ns() - began
    print(f"{name}: {reads + 1} reads over {wall // 1000} us: some total +{grew} us; "
          f"the spinners waited {ran // 1000} us for their CPU")


def main():
    ticks = int(sys.argv[1])
    allowed = sorted(os.sched_getaffinity(0))
    cpu = int(sys.argv[2]) if len(sys.argv) > 2 else allowed[0]
    others = [c for c in allowed if c != cpu]
    if len(sys.argv) > 3:
        reader = int(sys.argv[3])
    elif others:
        reader = others[0]
    else:
        raise SystemExit(f"one CPU allowed ({cpu}): the reader needs one of its own beside the spinners'")
    if reader == cpu:
        raise SystemExit(f"the reader's CPU {reader} is the spinners': it needs one of its own")
    tick_ns = round(time.clock_getres(COARSE) * 1e9)
    print(f"{os.uname().release} {os.uname().machine}, {len(allowed)} CPUs allowed, tick "
          f"{tick_ns} ns, spinners on CPU {cpu}, reader on CPU {reader}, {ticks} ticks an arm")
    os.sched_setaffinity(0, {reader})
    spinners = [spinner(cpu), spinner(cpu)]
    try:
        for name, tight in (("tight", True), ("tick", False), ("tight", True), ("tick", False)):
            arm(name, spinners, ticks, tick_ns, tight)
    finally:
        for pid in spinners:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)


main()
