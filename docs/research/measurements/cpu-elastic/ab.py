#!/usr/bin/env python3
"""What a run's CPU limit costs and gives (D191), in two builds of shards, each with a home
of its own that holds IMAGE already, rounds alternating which goes first. Each round stops
the home's daemon and removes its templates; then:

- a `run -d` of a command that sleeps, without a limit, boots and saves the image's
  template, and its warm microVMs are made (settled: the same VM processes for 5 s);
- for each count C, the first `run --rm --cpus C IMAGE true` (timed), which boots a
  template of its own where each count has one, then --starts more of each count and of
  none, the counts in turn (each timed from the request to the exit);
- for each count, a `run -d --cpus C` that sleeps: what its workload sees (nproc, the
  guest's online CPUs, its cgroup's cpuset and quota) and its VM process's memory (macOS:
  its physical footprint, proc_pid_rusage's ri_phys_footprint; Linux: smaps_rollup's Rss,
  Pss and Anonymous); and once the home's microVMs settle, how many VM processes it holds
  and their memory in all;
- a `run -d --cpus 1` that sleeps, then `update --cpus 4` (timed), and the CPU time its
  cgroup used while 6 busy loops ran for 2 s (cpu.stat's usage_usec), then `update --cpus
  1` and the same again.

    ab.py OLD NEW --homes OLD_HOME,NEW_HOME [--rounds N] [--warmup N] [--counts 1,4]
          [--starts N] [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries; each home holds IMAGE, which is not pulled again (a
registry limits the pulls it serves). The first --warmup rounds (1) are not counted: a
build's first VM process waits for its host to assess a binary it has not run before (PM
M166). Prints n, p50, p90, p99 and max of each, with host, OS, load and revision; with
--raw, each round's, on stderr. Each build's `shards` runs with an empty DOCKER_CONFIG."""
import argparse, ctypes, os, platform, re, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--rounds", type=int, default=10)
p.add_argument("--warmup", type=int, default=1)
p.add_argument("--counts", default="1,4")
p.add_argument("--starts", type=int, default=10)
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="cpu-elastic-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
builds = {name: (binary, home) for (name, binary), home in zip((("old", a.old), ("new", a.new)), homes)}
counts = [c for c in a.counts.split(",") if c]
MACOS = platform.system() == "Darwin"
SEES = "nproc; cat /sys/devices/system/cpu/online /sys/fs/cgroup/cpuset.cpus.effective /sys/fs/cgroup/cpu.max"
# The CPU time the run's cgroup uses while 6 busy loops run for 2 s, in µs.
BURN = ("u() { sed -n 's/^usage_usec //p' /sys/fs/cgroup/cpu.stat; }; a=$(u); p=; i=0; "
        "while [ $i -lt 6 ]; do (while :; do :; done) & p=\"$p $!\"; i=$((i + 1)); done; "
        "sleep 2; b=$(u); kill $p; echo $((b - a))")


def shards(build, *args, check=True):
    binary, home = builds[build]
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    t = time.perf_counter()
    done = subprocess.run([binary, *args], env=e, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    took = (time.perf_counter() - t) * 1000
    if check and done.returncode != 0:
        raise SystemExit(f"{build} {' '.join(args)}: {done.returncode}: {done.stderr.decode(errors='replace')}")
    return took, done.stdout.decode(errors="replace")


def vms(home):
    """The home's VM processes' PIDs."""
    out = subprocess.run(["ps", "-axww", "-o", "pid=,command="], capture_output=True, text=True).stdout
    found = []
    for line in out.splitlines():
        pid, _, command = line.strip().partition(" ")
        words = command.split()
        if len(words) >= 2 and words[0].endswith("shards-vm") and home in command:
            found.append(int(pid))
    return sorted(found)


if MACOS:
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib")

    class RusageV2(ctypes.Structure):
        # <sys/resource.h>, struct rusage_info_v2, to its physical footprint.
        _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [
            (f, ctypes.c_uint64)
            for f in ("user", "system", "idle", "interrupt", "pageins", "wired", "resident", "footprint")
        ] + [("rest", ctypes.c_uint64 * 16)]


def memory(pid):
    """A VM process's memory in MiB: macOS {footprint, resident}; Linux {rss, pss, anon}."""
    if MACOS:
        u = RusageV2()
        if libproc.proc_pid_rusage(pid, 2, ctypes.byref(u)) != 0:
            return None
        return {"footprint": u.footprint / 2**20, "resident": u.resident / 2**20}
    try:
        text = open(f"/proc/{pid}/smaps_rollup").read()
    except OSError:
        return None
    kib = {k: int(v) for k, v in re.findall(r"^(\w+):\s+(\d+) kB", text, re.M)}
    return {"rss": kib["Rss"] / 1024, "pss": kib["Pss"] / 1024, "anon": kib["Anonymous"] / 1024}


def settled(home, timeout=180):
    """Waits until the home's VM processes stay the same for 5 s, and returns them."""
    deadline = time.monotonic() + timeout
    last, since = None, time.monotonic()
    while time.monotonic() < deadline:
        now = vms(home)
        if now != last:
            last, since = now, time.monotonic()
        elif now and time.monotonic() - since >= 5:
            return now
        time.sleep(0.25)
    raise SystemExit(f"{home}: its microVMs never settled: {last}")


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {os.cpu_count()} CPUs, {platform.system()} "
      f"{platform.release()}, revision {rev}, load {os.getloadavg()}")
results = {b: {} for b in builds}
counted = True


def note(build, key, value):
    if value is not None and counted:
        results[build].setdefault(key, []).append(value)


n = 0
for i in range(a.warmup + a.rounds):
    counted = i >= a.warmup
    # Each build first in turn.
    for build in list(builds) if i % 2 else list(builds)[::-1]:
        binary, home = builds[build]
        shards(build, "daemon", "stop", check=False)
        templates = os.path.join(home, "templates")
        if os.path.isdir(templates):
            shutil.rmtree(templates)
        n += 1
        base = f"cpus-{os.getpid()}-{n}"
        shards(build, "run", "-d", "--name", base, a.image, "sleep", "100000")
        settled(home)
        raw = []
        for c in counts:
            took, _ = shards(build, "run", "--rm", "--cpus", c, a.image, "true")
            note(build, f"first run, --cpus {c}, ms", took)
            raw.append(f"first --cpus {c} {took:.1f}")
        settled(home)
        for _ in range(a.starts):
            for c in counts + [None]:
                flags = ["--cpus", c] if c else []
                took, _ = shards(build, "run", "--rm", *flags, a.image, "true")
                note(build, f"run, --cpus {c or 'none'}, ms", took)
        for c in counts + [None]:
            name = f"{base}-{c or 'none'}"
            flags = ["--cpus", c] if c else []
            shards(build, "run", "-d", "--name", name, *flags, a.image, "sleep", "100000")
            _, seen = shards(build, "exec", name, "sh", "-c", SEES)
            raw.append(f"--cpus {c or 'none'} sees {' | '.join(seen.split(chr(10))[:-1])}")
            time.sleep(1)
            _, pid = shards(build, "inspect", "-f", "{{.State.Pid}}", name)
            m = memory(int(pid.strip()))
            for k, v in (m or {}).items():
                note(build, f"run, --cpus {c or 'none'}, {k} MiB", v)
            shards(build, "rm", "-f", name)
        held = settled(home)
        total = {}
        for pid in held:
            for k, v in (memory(pid) or {}).items():
                total[k] = total.get(k, 0) + v
        note(build, "VM processes held", len(held))
        for k, v in total.items():
            note(build, f"VM processes' {k}, all, MiB", v)
        name = f"{base}-update"
        shards(build, "run", "-d", "--name", name, "--cpus", "1", a.image, "sleep", "100000")
        for cpus in ("4", "1"):
            took, _ = shards(build, "update", "--cpus", cpus, name)
            note(build, f"update --cpus {cpus} ms", took)
            _, used = shards(build, "exec", name, "sh", "-c", BURN)
            note(build, f"CPUs used after update --cpus {cpus}", int(used.strip()) / 2e6)
            raw.append(f"update {cpus} {took:.1f} ms, used {int(used.strip()) / 2e6:.2f} CPUs")
        shards(build, "rm", "-f", name, base)
        if a.raw:
            print(f"round {i} {build}: " + "; ".join(raw), file=sys.stderr)

for build in builds:
    for key, xs in results[build].items():
        print(f"{build} {key:<36} n {len(xs):>3} p50 {pct(xs, .5):9.2f} p90 {pct(xs, .9):9.2f} "
              f"p99 {pct(xs, .99):9.2f} max {max(xs):9.2f}")
print(f"load {os.getloadavg()}")
for build in builds:
    shards(build, "daemon", "stop", check=False)
