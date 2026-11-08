"""What a builder's ceiling gives a build and costs its host: the most memory a builder may
have at half its host's (the ceiling before PM M490, set as SHARDS_BUILD_MEMORY) against all
of it (D133's ceiling since), the same release build of `shards` in both arms, interleaved
so that both see the same host.

    python3 measure.py SHARDS_DIR HOME N OUT.json

SHARDS_DIR holds a release `shards` (and, on macOS, its signed `shards-vm`, which the run
starts through SHARDS_VM_BINARY). HOME is a SHARDS_HOME with alpine:3.22 pulled, which every
build takes its base from. DOCKER_CONFIG should name an empty directory.

Each turn builds, both arms in random order, `--no-cache --progress=plain`:

- `start`: `RUN true`, a builder's start and one step, which a ceiling it never reaches
  should cost nothing (its virtio-mem region is the ceiling's size; nothing of it is
  plugged);
- `quarter`: one step that holds a quarter of the host's memory in one anonymous buffer
  (busybox `dd bs=...M count=1 iflag=fullblock`, which allocates its block and fills it),
  within both ceilings: what plugging costs as a builder grows, the same in both arms;
- `past`: one step that holds half the host's memory and 2 GiB more, past the old ceiling:
  refused under it (its allocation, or 137), built under the new one where the host has
  the room;

and once, `meminfo`, each arm's `MemTotal` as a step sees it (D149).

A sample is the build's wall clock, spawn to exit, its exit status, the builder's peak
resident set (the VM process's SHARDS_TIMING line), and the host's least available memory
while it ran, sampled every 100 ms (Linux: /proc/meminfo's MemAvailable; macOS: vm_stat's
free, inactive, speculative and purgeable pages). The host's own use, its total less what
was available, is sampled before each turn. Reports n, p50, p90, p99 and max per arm and
build, and writes every sample to OUT.json.
"""
import json, os, platform, random, re, shutil, subprocess, sys, tempfile, threading, time

shards_dir, home, n, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
MAC = platform.system() == "Darwin"


def host_bytes():
    if MAC:
        return int(subprocess.check_output(["sysctl", "-n", "hw.memsize"]))
    return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGESIZE")


def available():
    if MAC:
        text = subprocess.check_output(["vm_stat"], text=True)
        page = int(re.search(r"page size of (\d+)", text).group(1))
        pages = 0
        for name in ("Pages free", "Pages inactive", "Pages speculative", "Pages purgeable"):
            m = re.search(name + r":\s+(\d+)", text)
            pages += int(m.group(1)) if m else 0
        return pages * page
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) << 10
    raise SystemExit("no MemAvailable")


HOST = host_bytes()
HOST_MIB = HOST >> 20
BLOCK_MIB = 128  # whole Linux memory blocks, what a builder is plugged in (D133)
HALF = HOST_MIB // 2 - (HOST_MIB // 2) % BLOCK_MIB
QUARTER = HOST_MIB // 4
PAST = HOST_MIB // 2 + 2048
DOCKERFILES = {
    "start": "FROM alpine:3.22\nRUN true\n",
    "quarter": f"FROM alpine:3.22\nRUN dd if=/dev/zero of=/dev/null bs={QUARTER}M count=1 iflag=fullblock\n",
    "past": f"FROM alpine:3.22\nRUN dd if=/dev/zero of=/dev/null bs={PAST}M count=1 iflag=fullblock\n",
    "meminfo": "FROM alpine:3.22\nRUN grep MemTotal /proc/meminfo\n",
}
ARMS = {"half": str(HALF), "all": None}
scratch = tempfile.mkdtemp(prefix="builder-ceiling-")
contexts = {}
for name, dockerfile in DOCKERFILES.items():
    ctx = os.path.join(scratch, name)
    os.makedirs(ctx)
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write(dockerfile)
    contexts[name] = ctx


def build(arm, name):
    # A host shared with others' work: a step that holds memory only where it leaves the
    # host 6 GiB; waited for a minute at most, then skipped.
    need = {"quarter": QUARTER, "past": PAST}.get(name, 0) + 6144
    for _ in range(60):
        if available() >> 20 >= need:
            break
        time.sleep(1)
    if available() >> 20 < need:
        return {"arm": arm, "build": name, "status": None, "killed": False, "wall_ms": None,
                "rss_mib": None, "least_available_mib": None, "memtotal_kib": None,
                "tail": "skipped: the host had not the room"}
    env = dict(os.environ, SHARDS_HOME=home, SHARDS_TIMING="1")
    env.pop("SHARDS_BUILD_MEMORY", None)
    if ARMS[arm] is not None:
        env["SHARDS_BUILD_MEMORY"] = ARMS[arm]
    vm = os.path.join(shards_dir, "shards-vm")
    if os.path.exists(vm):
        env["SHARDS_VM_BINARY"] = vm
    least = [available()]
    done_flag = threading.Event()

    def sample():
        while not done_flag.wait(0.1):
            least[0] = min(least[0], available())

    t = threading.Thread(target=sample)
    t.start()
    start = time.perf_counter()
    done = subprocess.run(
        [os.path.join(shards_dir, "shards"), "build", "--no-cache", "--progress=plain", contexts[name]],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    wall = time.perf_counter() - start
    done_flag.set()
    t.join()
    err = done.stderr.decode(errors="replace")
    timing = [json.loads(l.split("shards-timing ", 1)[1]) for l in err.splitlines() if "shards-timing " in l]
    total = re.search(r"MemTotal:\s+(\d+) kB", err)
    return {
        "arm": arm,
        "build": name,
        "status": done.returncode,
        "killed": "exit code: 137" in err,
        "wall_ms": wall * 1e3,
        "rss_mib": timing[-1]["rss_kib"] / 1024 if timing else None,
        "least_available_mib": least[0] >> 20,
        "memtotal_kib": int(total.group(1)) if total else None,
        "tail": err[-600:] if done.returncode not in (0,) and not "exit code: 137" in err else "",
    }


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


samples, own = [], []
for arm in ARMS:
    samples.append(build(arm, "meminfo"))
    print(json.dumps(samples[-1]), file=sys.stderr, flush=True)
for turn in range(-1, n):
    own.append((HOST - available()) >> 20)
    for name in ("start", "quarter", "past"):
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
print(f"host {HOST_MIB} MiB; half {HALF}; quarter step {QUARTER}; past step {PAST}")
print(f"host's own use before each turn: n={len(own)} p50={pct(own, .5):.0f} max={max(own)} MiB")
for s in samples:
    if s["build"] == "meminfo":
        print(f"meminfo {s['arm']}: MemTotal {s['memtotal_kib']} kB, status {s['status']}")
for name in ("start", "quarter", "past"):
    for arm in ARMS:
        xs = [s for s in samples if s["build"] == name and s["arm"] == arm]
        ok = sum(1 for s in xs if s["status"] == 0)
        skipped = sum(1 for s in xs if s["status"] is None)
        killed = sum(1 for s in xs if s["killed"])
        line = f"{name:8s} {arm:4s}: built {ok}/{len(xs)}, ended 137 {killed}, skipped {skipped}"
        for metric in ("wall_ms", "rss_mib", "least_available_mib"):
            v = [s[metric] for s in xs if s[metric] is not None]
            if v:
                line += f"; {metric} n={len(v)} p50={pct(v, .5):.0f} p90={pct(v, .9):.0f} p99={pct(v, .99):.0f} max={max(v):.0f}"
        print(line)
shutil.rmtree(scratch, ignore_errors=True)
