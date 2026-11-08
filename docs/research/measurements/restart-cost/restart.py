#!/usr/bin/env python3
"""What one restart of a microVM costs its host (PM M484): the host CPU time every process
of shards spends from a run's command ending to the next run's command starting (the
daemon, its spawner, the VM processes and network processes, warm ones refilled
included), and the wall time from the one to the other; and what a running microVM whose
command sleeps costs its host a second, idle.

    restart.py SHARDS IMAGE SCRATCH [--cold KERNEL INIT] [--microvms 5] [--restarts 10]
               [--idle 60] [--load N] [--trials 8]

SHARDS is a release build of `shards` (signed, on macOS, as build.rs signs what it
carries). IMAGE is pulled once into SCRATCH/home, which later runs reuse. With --cold,
runs boot KERNEL and INIT (SHARDS_KERNEL, SHARDS_INIT): no template; without, they
restore their image's template, as a user's do. --load N runs N busy loops beside it.

The daemon is this script's child (`shards run daemon`). Before it stops, its CPU time is
that of every process under it, each with the children it reaped (`tree_cpu`; the
clients this script runs are not in it, and the daemon's stop is in no life). wait4(2)
on the daemon would miss most: its spawner outlives it, and the VM processes are the
spawner's (it read 8 to 9 ms a run on Fedora where the cgroup read 78 to 82). On Linux
the daemon is started in a transient scope of the user's manager, whose cgroup's
`cpu.stat` counts every process in it, exited ones included, as a check of the first.

Each daemon's life starts with one run that pays its first costs (the template made,
the first time). Two lives with no more in them, one short and one `--idle` seconds
long, give a daemon's own fixed cost and its CPU a second idling (its warm VMs'
included), which are taken off the others' by their length.

The microVMs run `false` under `--restart on-failure:R`: each run ends at once, and the
daemon starts it again after its policy's wait. One run's CPU time, its end to the next
one's start, is the daemon tree's over that life, less the daemon's own, divided by the
runs. A microVM whose command sleeps is run for `--idle` seconds in another life: its CPU
a second, less the daemon's own and one run's. Each of `--trials` repeats the four
lives, and each figure is given as n, p50, p90, p99 and max of the trials'. The walls printed are from the home's
events: a run's `start` to its `die` (its command's start and end), and its `die` to the
next `start` (the policy's wait in it), and the same for each microVM's first restart,
whose wait is its shortest. Last, what the daemon learned (restart.rs): each restart's
cost as its log says it, and an idle microVM's as its home's `restart-costs` keeps it, to
set beside what this measured.
"""
import argparse
import json
import os
import re
import platform
import shutil
import statistics
import subprocess
import sys
import time


def run(shards, home, *args, env=None, check=True):
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=os.path.join(home, "..", "docker-config"))
    e.update(env or {})
    out = subprocess.run([shards, *args], env=e, capture_output=True, text=True)
    if check and out.returncode != 0:
        raise SystemExit(f"shards {' '.join(args)}: {out.returncode}: {out.stderr}")
    return out.stdout


def tree_cpu(root):
    """The CPU time, in seconds, of `root` and every process under it now, each with the
    children it has reaped: on Linux /proc's utime, stime, cutime and cstime (proc(5)), on
    macOS proc_pid_rusage(2)'s user, system and child times, in Mach time units
    (mach_timebase_info). A process gone and reaped by one outside the tree is missed."""
    if sys.platform.startswith("linux"):
        tick = os.sysconf("SC_CLK_TCK")
        procs = {}
        for d in os.listdir("/proc"):
            if not d.isdigit():
                continue
            try:
                f = open(f"/proc/{d}/stat").read().rsplit(")", 1)[1].split()
            except OSError:
                continue
            # After the command: state, ppid, ... utime is field 14, the 12th here.
            procs[int(d)] = (int(f[1]), sum(int(x) for x in f[11:15]) / tick)
    else:
        import ctypes
        libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")

        class Timebase(ctypes.Structure):
            _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]

        tb = Timebase()
        libc.mach_timebase_info(ctypes.byref(tb))
        # rusage_info_v2: uuid, then u64s: user, system, pkg_idle_wkups, interrupt_wkups,
        # pageins, wired, resident, phys_footprint, start, exit, child_user, child_system.
        buf = (ctypes.c_uint64 * 32)()
        procs = {}
        out = subprocess.run(["ps", "-A", "-o", "pid=,ppid="], capture_output=True, text=True).stdout
        for line in out.splitlines():
            pid, ppid = map(int, line.split())
            if libc.proc_pid_rusage(pid, 2, ctypes.byref(buf)) != 0:
                continue
            ticks = buf[2] + buf[3] + buf[12] + buf[13]
            procs[pid] = (ppid, ticks * tb.numer / tb.denom / 1e9)
    total, todo = 0.0, [root]
    while todo:
        pid = todo.pop()
        total += procs.get(pid, (0, 0.0))[1]
        todo.extend(c for c, (pp, _) in procs.items() if pp == pid)
    return total


def cgroup_cpu(pid):
    """The cgroup of `pid`'s usage, in seconds (Linux), or None."""
    try:
        path = open(f"/proc/{pid}/cgroup").read().strip().split("::", 1)[1]
        for line in open(f"/sys/fs/cgroup{path}/cpu.stat"):
            if line.startswith("usage_usec "):
                return int(line.split()[1]) / 1e6
    except (OSError, IndexError, ValueError):
        return None
    return None


def events(home):
    """Every event of the home's journal, oldest first."""
    out = []
    d = os.path.join(home, "events")
    for name in sorted(os.listdir(d), key=lambda n: int(n) if n.isdigit() else -1):
        if not name.isdigit():
            continue
        for line in open(os.path.join(d, name), errors="replace"):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out


def stats(xs):
    xs = sorted(xs)
    if not xs:
        return "n 0"
    q = lambda p: xs[min(len(xs) - 1, int(round(p * (len(xs) - 1))))]
    return (f"n {len(xs)}, p50 {q(0.5):.3f}, p90 {q(0.9):.3f}, p99 {q(0.99):.3f}, "
            f"max {xs[-1]:.3f}, mean {statistics.mean(xs):.3f}")


def daemon(shards, home, env):
    e = dict(os.environ, SHARDS_HOME=home, SHARDS_DAEMON_IDLE="100000",
             DOCKER_CONFIG=os.path.join(home, "..", "docker-config"))
    e.update(env)
    argv = [shards, "run", "daemon"]
    # systemd-run --scope execs the command itself once its scope is made: the daemon is
    # still this script's child.
    if sys.platform.startswith("linux") and shutil.which("systemd-run"):
        argv = ["systemd-run", "--user", "--scope", "--quiet", "-p", "CPUAccounting=yes",
                f"--unit=restart-cost-{os.getpid()}-{int(time.time() * 1000)}", *argv]
    log = open(os.path.join(home, "..", "daemon.out"), "a")
    p = subprocess.Popen(argv, env=e, stdout=log, stderr=log, stdin=subprocess.DEVNULL)
    # Its own pid file, before any client asks: a client asking first would start another.
    deadline = time.time() + 60
    pid_file = os.path.join(home, "daemon.pid")
    while time.time() < deadline:
        try:
            if int(open(pid_file).read().strip()) == p.pid:
                time.sleep(0.5)
                return p
        except (OSError, ValueError):
            pass
        if p.poll() is not None:
            raise SystemExit(f"the daemon exited: {p.returncode}")
        time.sleep(0.1)
    raise SystemExit("the daemon did not start")


def main():
    a = argparse.ArgumentParser()
    a.add_argument("shards")
    a.add_argument("image")
    a.add_argument("scratch")
    a.add_argument("--cold", nargs=2, metavar=("KERNEL", "INIT"))
    a.add_argument("--microvms", type=int, default=5)
    a.add_argument("--restarts", type=int, default=10)
    a.add_argument("--idle", type=int, default=60)
    a.add_argument("--load", type=int, default=0)
    a.add_argument("--trials", type=int, default=8)
    o = a.parse_args()
    shards = os.path.abspath(o.shards)
    scratch = os.path.abspath(o.scratch)
    home = os.path.join(scratch, "home")
    os.makedirs(home, mode=0o700, exist_ok=True)
    os.makedirs(os.path.join(scratch, "docker-config"), exist_ok=True)
    env = {}
    if o.cold:
        env = {"SHARDS_KERNEL": os.path.abspath(o.cold[0]), "SHARDS_INIT": os.path.abspath(o.cold[1])}
    run(shards, home, "stop", "daemon", check=False)
    if not run(shards, home, "images", "-q", o.image, check=False).strip():
        run(shards, home, "pull", "-q", o.image)
    run(shards, home, "stop", "daemon", check=False)
    loads = [subprocess.Popen([sys.executable, "-c", "while True: pass"]) for _ in range(o.load)]
    try:
        rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True,
                             text=True, cwd=os.path.dirname(os.path.abspath(__file__))).stdout.strip()
        print(f"host {platform.node()} {platform.machine()} {platform.platform()}, "
              f"{os.cpu_count()} CPUs, rev {rev}, {'cold boots' if o.cold else 'template restores'}, "
              f"load {o.load} busy loops", flush=True)
        def session(work):
            """A daemon's life with `work` in it, after a run that pays its first costs:
            its tree's CPU (`tree_cpu`) and its cgroup's, in seconds, and its length."""
            p = daemon(shards, home, env)
            t0 = time.time()
            run(shards, home, "run", "--rm", "--pull", "never", o.image, "true", env=env)
            work()
            cpu, cg = tree_cpu(p.pid), cgroup_cpu(p.pid)
            wall = time.time() - t0
            run(shards, home, "stop", "daemon", check=False)
            p.wait()
            return cpu, cg, wall

        def own(cpu, cg, wall, short, long):
            """CPU less a daemon's own for a life of `wall` seconds (rusage, cgroup)."""
            def less(x, s0, s1):
                if x is None or s0 is None or s1 is None:
                    return None
                rate = (s1[0] - s0[0]) / max(s1[1] - s0[1], 1e-9)
                return x - s0[0] - rate * (wall - s0[1])
            return (less(cpu, (short[0], short[2]), (long[0], long[2])),
                    less(cg, (short[1], short[2]), (long[1], long[2])))

        session(lambda: None)
        made = []
        idles, daemons, pers, pers_cg, idles_cg = [], [], [], [], []
        for trial in range(o.trials):
            short = session(lambda: None)
            long = session(lambda: time.sleep(o.idle))
            daemons.append((long[0] - short[0]) / (long[2] - short[2]))
            names = [f"loop-{os.getpid()}-{trial}-{i}" for i in range(o.microvms)]
            made.extend(names)
            timed = {}

            def loops():
                t0 = time.time()
                for n in names:
                    run(shards, home, "run", "-d", "--name", n, "--pull", "never", "--restart",
                        f"on-failure:{o.restarts}", o.image, "false", env=env)
                deadline = time.time() + 3600
                while True:
                    counts = [run(shards, home, "inspect", "-f",
                                  "{{.RestartCount}} {{.State.Status}}", n).split() for n in names]
                    if all(c == [str(o.restarts), "exited"] for c in counts):
                        break
                    if time.time() > deadline:
                        raise SystemExit(f"the loops did not end: {counts}")
                    time.sleep(0.5)
                timed["loop"] = time.time() - t0

            cpu, cg = own(*session(loops), short, long)
            runs = o.microvms * (o.restarts + 1)
            per = cpu / runs
            pers.append(per)
            per_cg = None if cg is None else cg / runs
            if per_cg is not None:
                pers_cg.append(per_cg)

            def idle():
                n = f"idle-{os.getpid()}-{trial}"
                run(shards, home, "run", "-d", "--name", n, "--pull", "never", o.image, "sleep",
                    "100000", env=env)
                time.sleep(o.idle)
                run(shards, home, "rm", "-f", n, check=False)

            cpu, cg = own(*session(idle), short, long)
            idles.append((cpu - per) / o.idle)
            if cg is not None and per_cg is not None:
                idles_cg.append((cg - per_cg) / o.idle)
            print(f"trial {trial}: {runs} runs of `false` in {timed['loop']:.1f} s, "
                  f"{per * 1e3:.2f} ms of host CPU each (process tree)"
                  + (f" (cgroup {per_cg * 1e3:.2f})" if per_cg is not None else "")
                  + f"; a microVM sleeping {idles[-1] * 1e3:.3f} ms a second"
                  + (f" (cgroup {idles_cg[-1] * 1e3:.3f})" if idles_cg else "")
                  + f"; the daemon idling {daemons[-1] * 1e3:.3f} ms a second", flush=True)
        ms = lambda xs: [x * 1e3 for x in xs]
        print(f"a run of `false`, its end to the next's start, host CPU (ms, process tree): {stats(ms(pers))}")
        if pers_cg:
            print(f"  the same by cgroup (ms): {stats(ms(pers_cg))}")
        print(f"a microVM whose command sleeps, host CPU a second (ms, process tree): {stats(ms(idles))}")
        if idles_cg:
            print(f"  the same by cgroup (ms): {stats(ms(idles_cg))}")
        print(f"the daemon idling, host CPU a second (ms, process tree): {stats(ms(daemons))}", flush=True)
        ev = events(home)
        lives, gaps, firsts = [], [], []
        for n in made:
            mine = [(e["Action"], e["timeNano"]) for e in ev
                    if e.get("Type") == "container"
                    and e.get("Actor", {}).get("Attributes", {}).get("name") == n
                    and e.get("Action") in ("start", "die")]
            for (a1, t1), (a2, t2) in zip(mine, mine[1:]):
                if a1 == "start" and a2 == "die":
                    lives.append((t2 - t1) / 1e6)
                elif a1 == "die" and a2 == "start":
                    if not gaps or len(gaps) % o.restarts == 0:
                        firsts.append((t2 - t1) / 1e6)
                    gaps.append((t2 - t1) / 1e6)
        print(f"a run of `false`, its start to its die (ms): {stats(lives)}", flush=True)
        print(f"a restart, its die to the next start, the policy's wait in it (ms): {stats(gaps)}",
              flush=True)
        print(f"a first restart, its die to the next start, its wait the shortest (ms): "
              f"{stats(firsts)}", flush=True)
        # What the daemon learned (restart.rs): each restart's cost as its log (its stderr)
        # says it, and the idle rate its home keeps, a least-squares slope (`Fit`).
        unit = {"ns": 1e-6, "µs": 1e-3, "ms": 1.0, "s": 1e3}
        learned = [float(n) * unit[u] for n, u in re.findall(
            r"restarts in .*?: its restart ([0-9.]+)(ns|µs|ms|s) of CPU",
            open(os.path.join(scratch, "daemon.out"), errors="replace").read())]
        print(f"learned by the daemon, a restart's cost (ms): {stats(learned[-len(gaps):])}")
        try:
            _, n, t, c, tt, tc = map(float, open(os.path.join(home, "restart-costs")).read().split())
            slope = (n * tc - t * c) / (n * tt - t * t)
            print(f"learned by the daemon, an idle microVM: {slope * 1e3:.3f} ms a second (slope), "
                  f"{c / t * 1e3:.3f} (its CPU over its time), over {int(n)} warm VMs, {t:.1f} s",
                  flush=True)
        except (OSError, ValueError, ZeroDivisionError):
            print("learned by the daemon, an idle microVM: none", flush=True)
        session(lambda: run(shards, home, "rm", "-f", *made, check=False))
    finally:
        for l in loads:
            l.kill()


if __name__ == "__main__":
    main()
