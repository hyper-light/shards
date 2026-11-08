#!/usr/bin/env python3
"""PM M462 (D215): what the guest kernel's BPF JIT costs a host's fleet of microVMs, beside
ab.py's costs inside one guest. One `shards`, two homes, each with IMAGE and a guest of its
own recorded (`shards configure guest --kernel K --init INIT`): A the kernel pinned before
D215 (interpreter), B the JIT kernel. Rounds alternate which home goes first. Each round,
in each home:

- the daemon stopped and the home's templates removed, so the round's first run boots its
  guest and saves the image's template (init attaches the device filter before the save,
  D44, so the JIT's pack is in the template): `run -d IMAGE sleep`, its client wall clock
  (`first`);
- --n - 1 more `run -d`, each served from the pool's warm microVMs, restored from that
  template, each one's client wall clock (`restored`): request to a workload running;
- once the home's VM processes stay the same for 5 s, each one's memory (Linux:
  /proc/PID/smaps_rollup's Rss, Pss and Anonymous; macOS: proc_pid_rusage's physical
  footprint and resident size), the booted one apart from the restored ones (the runs and
  the warm microVMs the pool keeps), and their sum; the template's disk blocks (du);
- every run removed (`rm -f`).

    fleet.py --shards SHARDS --init INIT --a KERNEL --b KERNEL --homes DIR
             [--n 10] [--rounds 10] [--warmup 1] [--image IMAGE]

DIR keeps both homes (DIR/A, DIR/B) between invocations: IMAGE is pulled into each once
(a registry limits the pulls it serves). INIT is the shards-init the guests run. Prints n,
p50, p90, p99 and max of each, with host, OS and revision. Every `shards` runs with an empty
DOCKER_CONFIG. Start latency through the daemon's pools, request to status, one run at a
time: build-ab/ab.py over the same two homes (ENV_* unset, each home's own guest).
"""
import argparse, atexit, ctypes, hashlib, math, os, platform, re, shutil, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
for flag in ("--shards", "--init", "--a", "--b", "--homes"):
    p.add_argument(flag, required=True)
p.add_argument("--n", type=int, default=10)
p.add_argument("--rounds", type=int, default=10)
p.add_argument("--warmup", type=int, default=1)
p.add_argument("--image", default="alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6")
a = p.parse_args()

MACOS = platform.system() == "Darwin"
work = tempfile.mkdtemp(prefix="bpf-fleet-")
config = os.path.join(work, "docker-config")
os.makedirs(config)
kernels = {"A": os.path.realpath(a.a), "B": os.path.realpath(a.b)}
homes = {k: os.path.join(os.path.realpath(a.homes), k) for k in kernels}


def shards(arm, *args, check=True):
    e = dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config)
    t = time.perf_counter()
    done = subprocess.run([a.shards, *args], env=e, stdin=subprocess.DEVNULL, capture_output=True)
    took = (time.perf_counter() - t) * 1000
    if check and done.returncode != 0:
        raise SystemExit(f"{arm} {' '.join(args)}: {done.returncode}: {done.stderr.decode(errors='replace')}")
    return took


@atexit.register
def clean():
    for arm in homes:
        shards(arm, "stop", "daemon", check=False)
    shutil.rmtree(work, ignore_errors=True)


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
    """A VM process's memory in KiB: macOS {footprint, resident}; Linux {rss, pss, anon}."""
    if MACOS:
        u = RusageV2()
        if libproc.proc_pid_rusage(pid, 2, ctypes.byref(u)) != 0:
            return None
        return {"footprint": u.footprint // 1024, "resident": u.resident // 1024}
    try:
        text = open(f"/proc/{pid}/smaps_rollup").read()
    except OSError:
        return None
    kib = {k: int(v) for k, v in re.findall(r"^(\w+):\s+(\d+) kB", text, re.M)}
    return {"rss": kib["Rss"], "pss": kib["Pss"], "anon": kib["Anonymous"]}


def settled(home, least, timeout=300):
    """The home's VM processes once at least `least` are there and none come or go for 5 s."""
    deadline = time.monotonic() + timeout
    last, since = None, time.monotonic()
    while time.monotonic() < deadline:
        now = sorted(vms(home))
        if now != last:
            last, since = now, time.monotonic()
        elif len(now) >= least and time.monotonic() - since >= 5:
            return now
        time.sleep(0.25)
    raise SystemExit(f"{home}: its microVMs never settled: {last}")


def du_kib(path):
    out = subprocess.run(["du", "-sk", path], capture_output=True, text=True).stdout.split()
    return int(out[0]) if out else 0


def pct(xs, q):
    # The nearest rank, as ab.py.
    xs = sorted(xs)
    return xs[min(len(xs), max(1, math.ceil(q * len(xs)))) - 1]


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:12]


for arm, kernel in kernels.items():
    os.makedirs(homes[arm], exist_ok=True)
    shards(arm, "configure", "guest", "--kernel", kernel, "--init", os.path.realpath(a.init))
    images = subprocess.run([a.shards, "images", "-q"], capture_output=True, text=True,
                            env=dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config)).stdout
    if not images.strip():
        shards(arm, "pull", a.image)

results = {arm: {} for arm in kernels}


def note(arm, key, value, counted):
    if counted and value is not None:
        results[arm].setdefault(key, []).append(value)


serial = 0
for i in range(a.warmup + a.rounds):
    counted = i >= a.warmup
    for arm in (list(kernels) if i % 2 == 0 else list(kernels)[::-1]):
        home = homes[arm]
        shards(arm, "stop", "daemon", check=False)
        shutil.rmtree(os.path.join(home, "templates"), ignore_errors=True)
        names = []
        for j in range(a.n):
            serial += 1
            name = f"jit-{os.getpid()}-{serial}"
            took = shards(arm, "run", "-d", "--pull", "never", "--name", name, a.image, "sleep", "100000")
            names.append(name)
            note(arm, "first run ms" if j == 0 else "restored run ms", took, counted)
        now = settled(home, a.n)
        sums = {}
        for pid, booted in now:
            m = memory(pid)
            if not m:
                continue
            for k, v in m.items():
                note(arm, f"{'booted' if booted else 'restored'} {k} KiB", v, counted)
                sums[k] = sums.get(k, 0) + v
        note(arm, "VMs", len(now), counted)
        for k, v in sums.items():
            note(arm, f"all VMs {k} KiB", v, counted)
            note(arm, f"all VMs {k} KiB per VM", v / len(now), counted)
        note(arm, "templates du KiB", du_kib(os.path.join(home, "templates")), counted)
        shards(arm, "rm", "-f", *names)

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True,
                     cwd=os.path.dirname(os.path.abspath(__file__))).stdout.strip()
u = platform.uname()
print(f"host {u.node} {u.machine} · {platform.platform()} · revision {rev} · {a.n} runs a round · "
      f"rounds {a.rounds} after {a.warmup} · image {a.image}")
for arm, kernel in kernels.items():
    print(f"{arm}: {os.path.basename(kernel)} sha256 {digest(kernel)}")
for key in results["A"]:
    for arm in kernels:
        xs = results[arm].get(key, [])
        if xs:
            print(f"{key:<30} {arm} n {len(xs):>4} p50 {pct(xs, .5):10.1f} p90 {pct(xs, .9):10.1f} "
                  f"p99 {pct(xs, .99):10.1f} max {max(xs):10.1f}")
