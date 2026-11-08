"""What a builder's ceiling gives a build and costs its host (D133, PM M490): the most memory
a builder may have, as shards derives it from the host it runs on and the builds its home
has recorded, against half its host's (the copied ceiling before, set here as
SHARDS_BUILD_MEMORY). The same release build of `shards` in every arm, interleaved, so that
all see the same host.

    python3 measure.py SHARDS_DIR HOME N OUT.json

SHARDS_DIR holds a release `shards`. HOME is a SHARDS_HOME with alpine:3.22 pulled, which
every build takes its base from. DOCKER_CONFIG should name an empty directory.

Arms, each build in random order within a turn:

- `half`: SHARDS_BUILD_MEMORY of half the host's memory, in whole blocks;
- `cold`: the home's record (`builder-memory`) removed first, so the ceiling is the cold
  start's, what the host has available as the builder starts;
- `short`: the record says a build was ended short of memory, so the ceiling is all the
  host's memory.

Builds, each `--no-cache --progress=plain`:

- `meminfo`: `MemTotal` as a step sees it (D149), the ceiling less what its kernel keeps;
- `start`: `RUN true`, a builder's start and one step, which a ceiling it never reaches
  should cost nothing (its virtio-mem region is the ceiling's size; nothing of it plugged);
- `quarter`: one step that holds a quarter of the host's memory in one buffer (busybox
  `dd bs=...M count=1 iflag=fullblock`, which allocates its block and fills it): what the
  build is recorded as needing against what it held;
- `past`: one step that holds half the host's memory and 2 GiB more, past the copied
  ceiling: refused under it, built under the others where the host has the room.

A sample is the build's wall clock, spawn to exit, its exit status, the host's available
memory just before it (as shards reads it: Linux's MemAvailable, macOS's
kern.memorystatus_level of hw.memsize), the builder's peak memory on its host (its VM
process's, a descendant of the build: macOS's phys_footprint, Linux's Pss) and the host's
least available memory while it ran, both sampled every 200 ms, and the need the build
recorded in its home (the record cleared before the build, read after; `half` and `short`
alike, the record kept as each arm needs it). Reports n, p50, p90, p99 and max per arm and
build, and writes every sample to OUT.json.
"""
import ctypes, json, os, platform, random, re, shutil, subprocess, sys, tempfile, threading, time

shards_dir, home, n, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
MAC = platform.system() == "Darwin"
RECORD = os.path.join(home, "builder-memory")


def host_bytes():
    if MAC:
        return int(subprocess.check_output(["sysctl", "-n", "hw.memsize"]))
    return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGESIZE")


HOST = host_bytes()


def available():
    """The host's available memory as shards reads it (build::host_available)."""
    if MAC:
        level = int(subprocess.check_output(["sysctl", "-n", "kern.memorystatus_level"]))
        return HOST // 100 * min(level, 100)
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) << 10
    raise SystemExit("no MemAvailable")


class RusageInfoV2(ctypes.Structure):
    """libproc's rusage_info_v2 (sys/resource.h), for its phys_footprint."""
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size",
        "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime", "child_user_time",
        "child_system_time", "child_pkg_idle_wkups", "child_interrupt_wkups", "child_pageins",
        "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten")]


LIBPROC = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True) if MAC else None


def memory_of(pid):
    """Process `pid`'s memory on this host, in bytes: macOS's phys_footprint, what its
    memory pressure counts; Linux's Pss. None once it is gone."""
    if MAC:
        info = RusageInfoV2()
        if LIBPROC.proc_pid_rusage(pid, 2, ctypes.byref(info)) != 0:
            return None
        return info.phys_footprint
    try:
        for line in open(f"/proc/{pid}/smaps_rollup"):
            w = line.split()
            if w and w[0] == "Pss:":
                return int(w[1]) << 10
    except OSError:
        return None
    return None


def vm_processes(root):
    """The VM processes under `root`, by pid (`shards-vm`, the builder's)."""
    ps = subprocess.run(["ps", "-axo", "pid=,ppid=,comm="], capture_output=True, text=True).stdout
    kids, names = {}, {}
    for line in ps.splitlines():
        w = line.split(None, 2)
        if len(w) == 3:
            kids.setdefault(int(w[1]), []).append(int(w[0]))
            names[int(w[0])] = os.path.basename(w[2])
    found, todo = [], [root]
    while todo:
        for kid in kids.get(todo.pop(), []):
            if names.get(kid, "").startswith("shards-vm"):
                found.append(kid)
            todo.append(kid)
    return found


HOST_MIB = HOST >> 20
BLOCK_MIB = 128  # whole Linux memory blocks, what a builder is plugged in (D133)
HALF = HOST_MIB // 2 - (HOST_MIB // 2) % BLOCK_MIB
QUARTER = HOST_MIB // 4
PAST = HOST_MIB // 2 + 2048
DOCKERFILES = {
    "meminfo": "FROM alpine:3.22\nRUN grep MemTotal /proc/meminfo\n",
    "start": "FROM alpine:3.22\nRUN true\n",
    "quarter": f"FROM alpine:3.22\nRUN dd if=/dev/zero of=/dev/null bs={QUARTER}M count=1 iflag=fullblock\n",
    "past": f"FROM alpine:3.22\nRUN dd if=/dev/zero of=/dev/null bs={PAST}M count=1 iflag=fullblock\n",
}
ARMS = ("half", "cold", "short")
scratch = tempfile.mkdtemp(prefix="builder-ceiling-")
contexts = {}
for name, dockerfile in DOCKERFILES.items():
    ctx = os.path.join(scratch, name)
    os.makedirs(ctx)
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write(dockerfile)
    contexts[name] = ctx


def read_record():
    try:
        text = open(RECORD).read()
    except FileNotFoundError:
        return None, None
    need = re.search(r"^needed (\d+)$", text, re.M)
    short = re.search(r"^short (\w+)$", text, re.M)
    return (int(need.group(1)) if need else None), (short.group(1) if short else None)


def build(arm, name):
    # A host shared with others' work: a step that holds memory only where it leaves the
    # host 6 GiB; waited for a minute at most, then skipped.
    need = {"quarter": QUARTER, "past": PAST}.get(name, 0) + 6144
    for _ in range(60):
        if available() >> 20 >= need:
            break
        time.sleep(1)
    if available() >> 20 < need:
        return {"arm": arm, "build": name, "status": None, "skipped": True}
    env = dict(os.environ, SHARDS_HOME=home)
    env.pop("SHARDS_BUILD_MEMORY", None)
    if arm == "half":
        env["SHARDS_BUILD_MEMORY"] = str(HALF)
    if arm == "short":
        with open(RECORD, "w") as f:
            f.write("needed 0\nshort yes\n")
    elif os.path.exists(RECORD):
        os.remove(RECORD)
    before = available()
    least = [before]
    peak = [0]
    start = time.perf_counter()
    proc = subprocess.Popen(
        [os.path.join(shards_dir, "shards"), "build", "--no-cache", "--progress=plain", contexts[name]],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    done_flag = threading.Event()

    def sample():
        while not done_flag.wait(0.2):
            least[0] = min(least[0], available())
            held = sum(m for m in (memory_of(p) for p in vm_processes(proc.pid)) if m)
            peak[0] = max(peak[0], held)

    t = threading.Thread(target=sample)
    t.start()
    err = proc.stderr.read().decode(errors="replace")
    proc.wait()
    wall = time.perf_counter() - start
    done_flag.set()
    t.join()
    total = re.search(r"MemTotal:\s+(\d+) kB", err)
    recorded, short = read_record()
    return {
        "arm": arm,
        "build": name,
        "status": proc.returncode,
        "skipped": False,
        "killed": "exit code: 137" in err,
        "out_of_memory": "out of memory" in err,
        "wall_ms": wall * 1e3,
        "available_before_mib": before >> 20,
        "vm_peak_mib": peak[0] >> 20 if peak[0] else None,
        "least_available_mib": least[0] >> 20,
        "memtotal_mib": int(total.group(1)) >> 10 if total else None,
        "need_mib": recorded >> 20 if recorded is not None else None,
        "short": short,
        "tail": err[-600:] if proc.returncode != 0 else "",
    }


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


samples, own = [], []
for turn in range(-1, n):
    own.append((HOST - available()) >> 20)
    for name in DOCKERFILES:
        arms = list(ARMS)
        random.shuffle(arms)
        for arm in arms:
            s = build(arm, name)
            s["turn"] = turn
            print(json.dumps(s), file=sys.stderr, flush=True)
            if turn >= 0:
                samples.append(s)
    print(f"turn {turn + 1}/{n}", file=sys.stderr, flush=True)

with open(out, "w") as f:
    json.dump({"host_mib": HOST_MIB, "half_mib": HALF, "quarter_mib": QUARTER, "past_mib": PAST,
               "host_own_mib": own, "samples": samples}, f, indent=1)
if MAC:
    cpu = subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip()
    osname = "macOS " + platform.mac_ver()[0]
else:
    cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "-")
    osname = platform.freedesktop_os_release().get("PRETTY_NAME", "Linux") + " " + platform.release()
print(f"host {platform.node()}; {osname}; {platform.machine()}; {cpu}; cpus {os.cpu_count()}")
print(f"host {HOST_MIB} MiB; half {HALF}; quarter step {QUARTER}; past step {PAST}")
print(f"host's own use before each turn: n={len(own)} p50={pct(own, .5):.0f} max={max(own)} MiB")
for name in DOCKERFILES:
    for arm in ARMS:
        xs = [s for s in samples if s["build"] == name and s["arm"] == arm]
        ran = [s for s in xs if not s["skipped"]]
        ok = sum(1 for s in ran if s["status"] == 0)
        refused = sum(1 for s in ran if s["killed"] or s["out_of_memory"])
        line = f"{name:8s} {arm:5s}: built {ok}/{len(xs)}, ended for memory {refused}, skipped {len(xs) - len(ran)}"
        for metric in ("wall_ms", "memtotal_mib", "available_before_mib", "need_mib", "vm_peak_mib",
                       "least_available_mib"):
            v = [s[metric] for s in ran if s.get(metric) is not None]
            if v:
                line += (f"; {metric} n={len(v)} p50={pct(v, .5):.0f} p90={pct(v, .9):.0f}"
                         f" p99={pct(v, .99):.0f} max={max(v):.0f}")
        print(line)
shutil.rmtree(scratch, ignore_errors=True)
