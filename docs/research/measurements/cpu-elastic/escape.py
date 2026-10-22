#!/usr/bin/env python3
"""How much of its host's CPU a workload that leaves its cgroup takes (D191, PM M289), in
builds of shards, each with a home of its own that holds IMAGE already. Each run: `run -d
--privileged --cgroupns host [--cpus 1] IMAGE sleep`, then, as the guest's root, `exec`
a shell that writes itself into the microVM's cgroup root (`/sys/fs/cgroup/cgroup.procs`,
which --cgroupns host shows it and --privileged makes writable), out of the run's
cpuset, and spins four threads for three seconds; the VM process's CPU time over the
exec, against its wall time. The builds, and with and without `--cpus 1`, alternate.

    escape.py NAME=SHARDS[,HOME] ... --home HOME [--runs N] [--image IMAGE]

Each NAME=SHARDS is a build (`held=/path/to/shards`), whose home is HOME unless it names
its own. Prints, for each build and limit, n, p50, p90, p99 and max of the CPUs used (CPU
time over wall time), with host, OS and kernel, and the cgroup each escape reached. Linux
reads the VM process's time from /proc/PID/stat, macOS from ps(1). Each build's `shards`
runs with an empty DOCKER_CONFIG."""
import argparse, os, platform, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("builds", nargs="+")
p.add_argument("--home", required=True)
p.add_argument("--runs", type=int, default=10)
p.add_argument("--image", default="alpine")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="cpu-escape-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
builds = {}
for spec in a.builds:
    name, rest = spec.split("=", 1)
    binary, _, home = rest.partition(",")
    builds[name] = (binary, os.path.realpath(home or a.home))
TICK = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100
ESCAPE = ("echo 0 > /sys/fs/cgroup/cgroup.procs && cat /proc/self/cgroup && "
          "for i in 1 2 3 4; do timeout 3 sh -c 'while :; do :; done' & done; wait")


def shards(name, *args):
    binary, home = builds[name]
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    return subprocess.run([binary, *args], env=e, capture_output=True, text=True)


def cpu_s(pid):
    if platform.system() == "Linux":
        fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / TICK
    t = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    s = 0.0
    for part in t.replace("-", ":").split(":"):
        s = s * 60 + float(part)
    return s


def once(name, limited):
    run = f"esc-{os.getpid()}"
    limit = ["--cpus", "1"] if limited else []
    started = shards(name, "run", "-d", "--name", run, "--privileged", "--cgroupns", "host", *limit, a.image, "sleep", "600")
    if started.returncode != 0:
        return None, f"run: {started.stderr.strip()[-200:]}"
    try:
        pid = int(shards(name, "inspect", "-f", "{{.State.Pid}}", run).stdout.strip())
        before, t0 = cpu_s(pid), time.perf_counter()
        done = shards(name, "exec", "-u", "root", run, "sh", "-c", ESCAPE)
        wall = time.perf_counter() - t0
        used = cpu_s(pid) - before
        if done.returncode != 0:
            return None, f"exec: {done.stderr.strip()[-200:]}"
        return (used / wall, done.stdout.strip().splitlines()[0] if done.stdout.strip() else "?"), None
    finally:
        shards(name, "rm", "-f", run)


def stats(xs):
    xs = sorted(xs)
    at = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    return len(xs), at(0.5), at(0.9), at(0.99), xs[-1]


def first(path, key):
    try:
        for line in open(path):
            if line.startswith(key):
                return line.split("=" if "=" in key else ":", 1)[1].strip().strip('"')
    except OSError:
        pass
    return "?"


used = {(b, l): [] for b in builds for l in (True, False)}
cgroups = {(b, l): set() for b in builds for l in (True, False)}
errors = []
try:
    for r in range(a.runs):
        names = list(builds) if r % 2 == 0 else list(reversed(builds))
        for b in names:
            for limited in ((True, False) if r % 2 == 0 else (False, True)):
                got, err = once(b, limited)
                if err:
                    errors.append(f"{b} {'--cpus 1' if limited else 'no limit'}: {err}")
                else:
                    used[(b, limited)].append(got[0])
                    cgroups[(b, limited)].add(got[1])
finally:
    for b in builds:
        shards(b, "daemon", "stop")
    shutil.rmtree(work, ignore_errors=True)

print(f"host: {platform.node()}, {first('/proc/cpuinfo', 'model name')}; "
      f"{first('/etc/os-release', 'PRETTY_NAME=')}, kernel {platform.release()}; {os.cpu_count()} CPUs; "
      f"image {a.image}; {a.runs} runs each, alternating")
print(f"{'build':10} {'limit':9} {'n':>3} {'p50':>6} {'p90':>6} {'p99':>6} {'max':>6}  CPUs used; escaped to")
for (b, limited), xs in used.items():
    if xs:
        n, p50, p90, p99, mx = stats(xs)
        print(f"{b:10} {'--cpus 1' if limited else 'none':9} {n:3} {p50:6.2f} {p90:6.2f} {p99:6.2f} {mx:6.2f}  "
              f"{', '.join(sorted(cgroups[(b, limited)]))}")
for e in errors:
    print("FAILED", e)
