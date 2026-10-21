#!/usr/bin/env python3
"""What a template's microVM, which goes on as its image's first run, holds of its writable
layer's head (D190), in two builds of shards, each with a home of its own that holds IMAGE
already, rounds alternating which goes first. Each round stops the home's daemon and
removes its templates, so that the next run boots and saves its image's template: a
`run -d` of a command that sleeps is that first run (timed). Once its template's warm
microVMs are made and stay, the memory of each VM process is read: the run's, which booted,
and the warm microVMs', restored from its template (macOS: its physical footprint,
proc_pid_rusage's ri_phys_footprint, which vmmap's summary reads too; Linux:
/proc/PID/smaps_rollup's Rss, Pss and Anonymous). Then a program copied into the run's
root, the layer, is run from there (`exec`, timed), its pages mapped from the layer and so
its page structures, the head, written; and the run's memory read again.

    footprint.py OLD NEW --homes OLD_HOME,NEW_HOME [--rounds N] [--warmup N] [--image IMAGE] [--raw]

OLD and NEW are `shards` binaries; each home holds IMAGE, which is not pulled again (a
registry limits the pulls it serves). The first --warmup rounds (1) are not counted: a
build's first VM process waits for its host to assess a binary it has not run before (PM
M166). Prints n, p50, p90, p99 and max of each, with host, OS
and revision; with --raw, each round's, on stderr. Each build's `shards` runs with an empty
DOCKER_CONFIG."""
import argparse, ctypes, os, platform, re, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("old")
p.add_argument("new")
p.add_argument("--homes", required=True)
p.add_argument("--rounds", type=int, default=10)
p.add_argument("--warmup", type=int, default=1)
p.add_argument("--image", default="alpine")
p.add_argument("--raw", action="store_true")
a = p.parse_args()

work = tempfile.mkdtemp(prefix="layer-head-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
homes = [os.path.realpath(h) for h in a.homes.split(",")]
builds = {name: (binary, home) for (name, binary), home in zip((("old", a.old), ("new", a.new)), homes)}
MACOS = platform.system() == "Darwin"


def shards(build, *args, check=True):
    binary, home = builds[build]
    # Its VMs' own times in its log (SHARDS_LOG=info): the template's save, its head's mapping back.
    e = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config, SHARDS_LOG="info")
    t = time.perf_counter()
    done = subprocess.run([binary, *args], env=e, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    took = (time.perf_counter() - t) * 1000
    if check and done.returncode != 0:
        raise SystemExit(f"{build} {' '.join(args)}: {done.returncode}: {done.stderr.decode(errors='replace')}")
    return took, done.stdout.decode(errors="replace")


def vms(home):
    """The home's VM processes: (pid, booted), booted for one that runs a kernel."""
    out = subprocess.run(["ps", "-axww", "-o", "pid=,command="], capture_output=True, text=True).stdout
    found = []
    for line in out.splitlines():
        pid, _, command = line.strip().partition(" ")
        words = command.split()
        if len(words) < 2 or not words[0].endswith("shards-vm") or home not in command:
            continue
        if words[1] == "run" and "--kernel" in words:
            found.append((int(pid), True))
        elif words[1] == "restore":
            found.append((int(pid), False))
    return found


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
    """Waits until the home's run VM and its warm VMs are there and stay for 5 s."""
    deadline = time.monotonic() + timeout
    last, since = None, time.monotonic()
    while time.monotonic() < deadline:
        now = sorted(vms(home))
        if now != last:
            last, since = now, time.monotonic()
        elif any(b for _, b in now) and any(not b for _, b in now) and time.monotonic() - since >= 5:
            return now
        time.sleep(0.25)
    raise SystemExit(f"{home}: its microVMs never settled: {last}")


def said(home, since, pattern):
    """The last number `pattern` finds in the home's daemon.log past offset `since`."""
    try:
        with open(os.path.join(home, "daemon.log"), "rb") as f:
            f.seek(since)
            text = f.read().decode(errors="replace")
    except OSError:
        return None
    found = re.findall(pattern, text)
    return int(found[-1]) if found else None


def log_end(home):
    try:
        return os.path.getsize(os.path.join(home, "daemon.log"))
    except OSError:
        return 0


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, revision {rev}")
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
        n += 1
        binary, home = builds[build]
        shards(build, "daemon", "stop", check=False)
        templates = os.path.join(home, "templates")
        if os.path.isdir(templates):
            shutil.rmtree(templates)
        name = f"head-{n}"
        since = log_end(home)
        took, _ = shards(build, "run", "-d", "--name", name, a.image, "sleep", "100000")
        note(build, "first run ms", took)
        now = settled(home)
        run = [pid for pid, booted in now if booted]
        warm = [m for m in (memory(pid) for pid, booted in now if not booted) if m]
        ran = memory(run[0])
        for k, v in (ran or {}).items():
            note(build, f"run {k} MiB", v)
        # The round's median warm microVM.
        for k in warm[0] if warm else ():
            note(build, f"warm {k} MiB", sorted(w[k] for w in warm)[len(warm) // 2])
        note(build, "warm VMs", len(warm))
        saved = said(home, since, r"snapshot written to \S+ in (\d+) us")
        remap = said(home, since, r"head mapped back from its snapshot in (\d+) us")
        note(build, "template written ms", saved / 1000 if saved is not None else None)
        note(build, "head remap ms", remap / 1000 if remap is not None else None)
        # busybox runs the applet its name says.
        shards(build, "exec", name, "cp", "/bin/busybox", "/true")
        execs = [shards(build, "exec", name, "/true")[0] for _ in range(2)]
        note(build, "exec from layer ms", execs[0])
        note(build, "exec again ms", execs[1])
        after = memory(run[0])
        for k, v in (after or {}).items():
            note(build, f"run after exec {k} MiB", v)
        if a.raw:
            print(f"round {i} {build} first run {took:.1f} ms, written {saved} us, remap {remap} us, "
                  f"execs {execs[0]:.2f} {execs[1]:.2f} ms, run {ran} after {after} warm {warm}", file=sys.stderr)
        shards(build, "rm", "-f", name)

for build in builds:
    for key, xs in results[build].items():
        print(f"{build} {key:<26} n {len(xs):>3} p50 {pct(xs, .5):9.2f} p90 {pct(xs, .9):9.2f} "
              f"p99 {pct(xs, .99):9.2f} max {max(xs):9.2f}")
for build in builds:
    shards(build, "daemon", "stop", check=False)
