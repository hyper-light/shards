#!/usr/bin/env python3
"""What a builder gives back under its host's memory pressure, and what that costs its build
(the plan "Builders give back what they hold cold and unplug under memory pressure", step 4;
D133, D188): builder-cold's builds (rust, npm, apt), each built in rounds, each round twice,
free and held, the arms' order turning each round, its steps run again each round for a new
ROUND, their caches kept (PM M141), by a build of shards with builder-cold's cold.patch (the
builder's own samples on its console) and give.patch (each give-back's answer, and how long
after its word it came, on the build's stderr).

- Linux: each build in a scope of the user's manager (systemd-run --user --scope) that swaps
  nothing (MemorySwapMax=0). Held, its MemoryHigh is the free arm's peak in the round
  before it (the scope's memory.peak) less what that arm's builder held at most that it
  could give back (its inactive_file and what lay plugged past twice what it used, at once):
  so the build fits its budget where its builder gives that back, and is held to it,
  stalling as its host reclaims what it cannot swap (PSI), which the build's watcher hears.
  A round's held arm comes after its free arm where the round is odd, before it where even,
  with the round before's peak: round 1's free arm alone fills the builds' caches.
- macOS, where no unprivileged process may raise the system's pressure: held, the build is
  told every 2 s that its host is short of memory (SHARDS_BUILD_PRESSURE's FIFO), as its
  watcher hears the system's pressure again every 2 s while it stays raised.

Beside it, each second, the builder's VM process on this host (phys_footprint on macOS, Pss
and RssAnon on Linux) and, on Linux, the scope's memory.current and memory.peak. Into
OUT/out.txt: a header, then `guest`, `host`, `give` and `build` lines, KEY=VALUE each, for
tables.py; into OUT/raw/, each build's stderr and its builder's console. Where the room left
on OUT's or HOME's volume falls under 5 GiB, the build under way is ended and the run with it.

    measure.py SHARDS OUT [--builds rust,npm,apt] [--rounds 5] [--home HOME] [--base REGISTRY]
"""
import argparse, ctypes, os, platform, re, signal, subprocess, sys, threading, time

here = os.path.dirname(os.path.realpath(__file__))
builds_dir = os.path.join(os.path.dirname(here), "builder-cold", "builds")
p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("out")
p.add_argument("--builds", default="rust,npm,apt")
p.add_argument("--rounds", type=int, default=5)
p.add_argument("--home")
p.add_argument("--base", help="the registry and repository the builds' bases come from (their BASE)")
a = p.parse_args()

shards = os.path.realpath(a.shards)
out = os.path.realpath(a.out)
os.makedirs(os.path.join(out, "raw"), exist_ok=True)
home = os.path.realpath(a.home) if a.home else os.path.join(out, "home")
config = os.path.join(out, "docker-config")
os.makedirs(config, exist_ok=True)
OUT = open(os.path.join(out, "out.txt"), "w")
macos = sys.platform == "darwin"
MIB = 1 << 20
# This run's own, in each round's ROUND: no step of a run before in the same home answers
# one of this run's from its cache.
RUN = int(time.time())
# How often a held build on macOS is told its host is short of memory: the watcher's
# REPEAT (platform/pressure.rs).
TOLD_EVERY = 2.0
lock = threading.Lock()


def say(line):
    with lock:
        print(line, flush=True)
        OUT.write(line + "\n")
        OUT.flush()


def header():
    rev = subprocess.run(["git", "-C", here, "rev-parse", "--short", "HEAD"], capture_output=True,
                         text=True).stdout.strip() or "unknown"
    if macos:
        cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
        osname = "macOS " + platform.mac_ver()[0]
        mem = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout.strip() or 0)
    else:
        info = open("/proc/cpuinfo").read()
        cpu = next((l.split(":", 1)[1].strip() for l in info.splitlines() if l.startswith("model name")), "-")
        osname = platform.freedesktop_os_release().get("PRETTY_NAME", "Linux")
        mem = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    say(f"host {platform.node()}; {osname}; kernel {platform.release()}; {platform.machine()}; {cpu}; "
        f"shards {rev}; memory {mem >> 20} MiB; cpus {os.cpu_count()}")
    say(f"builds {a.builds}; rounds {a.rounds}; home {home}; bases {a.base or 'each Dockerfiles own'}")


ROOM = 5 << 30
short = threading.Event()


def room():
    """The free bytes of the volumes the run writes to: its output's and its home's."""
    def free(path):
        while not os.path.exists(path):
            path = os.path.dirname(path)
        st = os.statvfs(path)
        return st.f_bavail * st.f_frsize
    return min(free(out), free(home))


class RusageInfoV2(ctypes.Structure):
    """libproc's rusage_info_v2 (sys/resource.h), for its phys_footprint."""
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size",
        "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime", "child_user_time",
        "child_system_time", "child_pkg_idle_wkups", "child_interrupt_wkups", "child_pageins",
        "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten")]


LIBPROC = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True) if macos else None


def footprint(pid):
    """The VM process's memory on this host, as KEY=VALUE fields; none once it is gone."""
    if macos:
        info = RusageInfoV2()
        if LIBPROC.proc_pid_rusage(pid, 2, ctypes.byref(info)) != 0:
            return None
        return f"footprint={info.phys_footprint}"
    try:
        fields = {}
        for line in open(f"/proc/{pid}/smaps_rollup"):
            w = line.split()
            if w and w[0] == "Pss:":
                fields["pss"] = int(w[1]) << 10
        for line in open(f"/proc/{pid}/status"):
            w = line.split()
            if w and w[0] == "RssAnon:":
                fields["rss_anon"] = int(w[1]) << 10
    except OSError:
        return None
    return " ".join(f"{k}={v}" for k, v in fields.items())


def descendants(root):
    """Each process under `root`, by pid, with its name."""
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
            found.append((kid, names.get(kid, "")))
            todo.append(kid)
    return found


def scope_of(pid):
    """The cgroup directory of process `pid` (Linux), none where it has gone."""
    try:
        for line in open(f"/proc/{pid}/cgroup"):
            if line.startswith("0::"):
                return "/sys/fs/cgroup" + line[3:].strip()
    except OSError:
        return None
    return None


def sample_host(build_pid, tag, stop, peak):
    """The builder's VM process's memory each second, and its scope's (Linux), until `stop`;
    the scope's memory.peak kept in `peak`; the build ended where the room runs short."""
    while not stop.wait(1):
        left = room()
        if left < ROOM:
            say(f"room {left >> 20} MiB: ending the run")
            short.set()
            os.kill(build_pid, signal.SIGTERM)
            return
        fields = []
        vm = next((pid for pid, name in descendants(build_pid) if name == "shards-vm"), None)
        if vm is not None:
            got = footprint(vm)
            if got:
                fields.append(f"pid={vm} {got}")
        scope = None if macos else scope_of(build_pid)
        if scope:
            try:
                current = int(open(f"{scope}/memory.current").read())
                peak[0] = max(peak[0], int(open(f"{scope}/memory.peak").read()))
                fields.append(f"scope_current={current} scope_peak={peak[0]}")
            except (OSError, ValueError):
                pass
        if fields:
            say(f"host t={time.time():.3f} {tag} {' '.join(fields)}")


def tell(path, stop):
    """Tells a held build every TOLD_EVERY s that its host is short of memory (macOS)."""
    with open(path, "w") as fifo:
        while not stop.wait(TOLD_EVERY):
            try:
                fifo.write("\n")
                fifo.flush()
            except OSError:
                return


GUEST = re.compile(r"builder-cold (.*)$")
GIVE = re.compile(r"builder-give (.*)$")
STEP = re.compile(r"^#(\d+) \[")


def tail(path, tag, stop, now_step, held_cold):
    """The builder's console as it is written, each whole line stamped as it is read, until
    `stop` and the file's end; the most it held at once that it could give back kept in
    `held_cold` (its inactive_file and what lay plugged past twice what it used)."""
    part = ""
    with open(path, errors="replace") as f:
        while True:
            chunk = f.read()
            if not chunk:
                if stop.is_set():
                    return
                time.sleep(0.01)
                continue
            *lines, part = (part + chunk).split("\n")
            now = time.time()
            for line in lines:
                m = GUEST.search(line)
                if not m:
                    continue
                words = m.group(1).split()
                end = words[-1] == "step_end"
                pairs = dict(zip((words[:-1] if end else words)[::2], (words[:-1] if end else words)[1::2]))
                try:
                    cold = int(pairs.get("inactive_file", 0)) + int(pairs.get("past_twice_used", 0))
                    held_cold[0] = max(held_cold[0], cold)
                except ValueError:
                    pass
                fields = " ".join(f"{k}={v}" for k, v in pairs.items())
                say(f"guest t={now:.3f} {tag} step={now_step[0]} step_end={int(end)} {fields}")


def one(r, name, arm, budget):
    """A build of `name` in round `r`, `arm` free or held (to `budget` bytes on Linux): its
    scope's peak and what its builder held at most that it could give back."""
    tag = f"round={r} build={name} arm={arm}"
    ctx = os.path.join(builds_dir, name)
    console = os.path.join(out, "raw", f"{r}-{name}-{arm}-console.txt")
    open(console, "w").close()
    env = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config, SHARDS_BUILDER_CONSOLE=console)
    told = None
    if macos and arm == "held":
        told = os.path.join(out, "raw", f"{r}-{name}-pressure")
        if os.path.exists(told):
            os.remove(told)
        os.mkfifo(told, 0o600)
        env["SHARDS_BUILD_PRESSURE"] = told
    base = ["--build-arg", f"BASE={a.base}"] if a.base else []
    cmd = [shards, "build", "--progress", "plain", "-t", f"builder-pressure-{name}",
           "--build-arg", f"ROUND={RUN}-{r}-{arm}", *base, "-f", os.path.join(ctx, "Dockerfile"), ctx]
    if not macos:
        scope = ["systemd-run", "--user", "--scope", "--quiet", "-p", "MemorySwapMax=0"]
        if arm == "held":
            scope += ["-p", f"MemoryHigh={budget}"]
        cmd = scope + cmd
    started = time.time()
    raw = open(os.path.join(out, "raw", f"{r}-{name}-{arm}.txt"), "w")
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                            errors="replace")
    stop = threading.Event()
    step, peak, held_cold = ["-"], [0], [0]
    threads = [threading.Thread(target=sample_host, args=(proc.pid, tag, stop, peak)),
               threading.Thread(target=tail, args=(console, tag, stop, step, held_cold))]
    if told:
        threads.append(threading.Thread(target=tell, args=(told, stop)))
    for t in threads:
        t.start()
    for line in proc.stdout:
        now = time.time()
        raw.write(f"{now:.3f} {line}")
        m = STEP.match(line)
        if m:
            step[0] = m.group(1)
        m = GIVE.search(line)
        if m:
            words = m.group(1).split()
            fields = " ".join(f"{k}={v}" for k, v in zip(words[::2], words[1::2]))
            say(f"give t={now:.3f} {tag} {fields}")
    code = proc.wait()
    stop.set()
    if told:
        # A writer waiting for its reader to open the FIFO, the build gone: opened here.
        try:
            os.close(os.open(told, os.O_RDONLY | os.O_NONBLOCK))
        except OSError:
            pass
    for t in threads:
        t.join()
    raw.close()
    say(f"build {tag} exit={code} s={time.time() - started:.1f} budget={budget} scope_peak={peak[0]} "
        f"held_cold={held_cold[0]}")
    for args in (["rmi", "-f", f"builder-pressure-{name}"], ["image", "prune", "-f"]):
        subprocess.run([shards, *args], env=env, capture_output=True)
    return peak[0], held_cold[0]


header()
builds = a.builds.split(",")
seen = {}
for r in range(1, a.rounds + 1):
    order = builds[(r - 1) % len(builds):] + builds[:(r - 1) % len(builds)]
    for name in order:
        arms = ("free", "held") if r % 2 == 1 or name not in seen else ("held", "free")
        for arm in arms:
            if short.is_set() or room() < ROOM:
                say(f"room {room() >> 20} MiB: ending the run")
                sys.exit(1)
            budget = 0
            if arm == "held" and not macos:
                peak, cold = seen.get(name, (0, 0))
                budget = max(peak - cold, 0)
            got = one(r, name, arm, budget)
            if arm == "free":
                seen[name] = got
say("done")
