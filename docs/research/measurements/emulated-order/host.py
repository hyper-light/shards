#!/usr/bin/env python3
"""M241's host side, as it measured M241 (emulation/host.py), here M250's: run.sh's runs, each measured from the host: ROUNDS rounds, each a pair of each
workload, natively then emulated, on each side in turn. A run's wall time is the guest's
(time -p). Its cost is its VM process's, sampled as serve.sh starts it and after it ends: CPU
time, the time its threads waited for a CPU, and on macOS the instructions and cycles they
retired (proc_pid_rusage, RUSAGE_INFO_V6), on Linux /proc's schedstat. shards' guests have no
steal clock (D179), so a guest charges a task with the time its vCPU waited for the host: the
host's figures are what a run cost, whatever the host's load.

Prints each run, each side's idle cost, then for each side and workload n, p50, p90, p99 and
max of the pairs' ratios, emulated over native, and of the emulated runs' waits.

    host.py ROUNDS SIDE PID COMMAND [SIDE PID COMMAND]
"""
import collections
import ctypes
import os
import shlex
import subprocess
import sys
import time

WORKLOADS = ("sha256", "gzip", "starts")

if sys.platform == "darwin":
    # struct rusage_info_v6, <sys/resource.h>.
    FIELDS = (
        "user system pkg_idle_wkups interrupt_wkups pageins wired_size resident_size "
        "phys_footprint proc_start_abstime proc_exit_abstime child_user_time "
        "child_system_time child_pkg_idle_wkups child_interrupt_wkups child_pageins "
        "child_elapsed_abstime diskio_bytesread diskio_byteswritten cpu_time_qos_default "
        "cpu_time_qos_maintenance cpu_time_qos_background cpu_time_qos_utility "
        "cpu_time_qos_legacy cpu_time_qos_user_initiated cpu_time_qos_user_interactive "
        "billed_system_time serviced_system_time logical_writes lifetime_max_phys_footprint "
        "instructions cycles billed_energy serviced_energy interval_max_phys_footprint "
        "runnable flags user_ptime system_ptime pinstructions pcycles energy_nj penergy_nj "
        "secure_time_in_system secure_ptime_in_system neural_footprint "
        "lifetime_max_neural_footprint interval_max_neural_footprint"
    ).split()

    class Usage(ctypes.Structure):
        _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(f, ctypes.c_uint64) for f in FIELDS] + [
            ("reserved", ctypes.c_uint64 * 9)
        ]

    class Timebase(ctypes.Structure):
        _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]

    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    timebase = Timebase()
    libc.mach_timebase_info(ctypes.byref(timebase))
    # Its times are in mach absolute time units.
    TICK = timebase.numer / timebase.denom / 1e9

    def sample(pid):
        u = Usage()
        if libc.proc_pid_rusage(pid, 6, ctypes.byref(u)) != 0:
            raise OSError(ctypes.get_errno(), f"proc_pid_rusage({pid})")
        cpu = (u.user + u.system) * TICK
        # Runnable: running, or waiting for a CPU.
        return {"cpu": cpu, "waited": u.runnable * TICK - cpu, "instructions": u.instructions, "cycles": u.cycles}

else:

    def sample(pid):
        cpu = waited = 0.0
        for task in os.listdir(f"/proc/{pid}/task"):
            try:
                with open(f"/proc/{pid}/task/{task}/schedstat") as f:
                    on, wait, _ = f.read().split()
            except FileNotFoundError:
                continue
            cpu += int(on) / 1e9
            waited += int(wait) / 1e9
        return {"cpu": cpu, "waited": waited}


class Side:
    def __init__(self, name, pid, command):
        self.name, self.pid = name, int(pid)
        self.serve = subprocess.Popen(
            shlex.split(command), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1
        )

    def line(self):
        line = self.serve.stdout.readline()
        if not line:
            raise RuntimeError(f"{self.name}: serve.sh ended")
        return line.split()

    def run(self, what):
        print(what, file=self.serve.stdin, flush=True)
        if self.line() != ["B"]:
            raise RuntimeError(f"{self.name} {what}: no B")
        before = sample(self.pid)
        print("go", file=self.serve.stdin, flush=True)
        answer = self.line()
        after = sample(self.pid)
        if answer[:1] != ["E"] or answer[1:2] == ["FAIL"] or len(answer) != 4:
            raise RuntimeError(f"{self.name} {what}: {' '.join(answer)}")
        run = {k: after[k] - before[k] for k in before}
        run["wall"] = float(answer[1])
        run["guest cpu"] = float(answer[2]) + float(answer[3])
        return run

    def idle(self, seconds):
        before = sample(self.pid)
        time.sleep(seconds)
        after = sample(self.pid)
        return {k: (after[k] - before[k]) / seconds for k in before}


def stats(xs, digits=1):
    xs = sorted(xs)
    pick = lambda q: xs[min(len(xs) - 1, int(len(xs) * q))]
    f = lambda x: f"{x:.{digits}f}"
    return f"n={len(xs)} p50={f(pick(0.5))} p90={f(pick(0.9))} p99={f(pick(0.99))} max={f(xs[-1])}"


def main():
    rounds = int(sys.argv[1])
    args = sys.argv[2:]
    sides = [Side(*args[i : i + 3]) for i in range(0, len(args), 3)]
    for side in sides:
        rates = side.idle(2.0)
        print(f"{side.name} idle, a second: " + " ".join(f"{k} {v:.3g}" for k, v in rates.items()), flush=True)
    runs = collections.defaultdict(list)
    for _ in range(rounds):
        for workload in WORKLOADS:
            for side in sides:
                for mode in ("native", "emulated"):
                    run = side.run(f"{mode}-{workload}")
                    runs[side.name, workload, mode].append(run)
                    shown = " ".join(f"{k} {v:.3f}" if isinstance(v, float) else f"{k} {v}" for k, v in run.items())
                    print(f"{side.name} {mode}-{workload} {shown}", flush=True)
    for side in sides:
        for workload in WORKLOADS:
            native, emulated = runs[side.name, workload, "native"], runs[side.name, workload, "emulated"]
            ratio = lambda k: [e[k] / n[k] for n, e in zip(native, emulated) if n[k] > 0]
            measures = ["wall", "cpu"] + [k for k in ("cycles", "instructions") if k in native[0]]
            shown = "; ".join(f"{k} ratio {stats(ratio(k))}" for k in measures)
            walls = lambda rs: sorted(r["wall"] for r in rs)[len(rs) // 2]
            print(
                f"{side.name} {workload}: {shown}; wall p50 {walls(native):.2f} s native, "
                f"{walls(emulated):.2f} s emulated; emulated waited, s: {stats([e['waited'] for e in emulated], 2)}"
            )


main()
