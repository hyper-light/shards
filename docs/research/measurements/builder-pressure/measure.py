#!/usr/bin/env python3
"""What a builder gives back under its host's memory pressure, and what that costs its build
(the plan "Builders give back what they hold cold and unplug under memory pressure", step 4;
D133, D188): builder-cold's builds (rust, npm, apt), each built in rounds, each round twice,
free and held, the arms' order turning each round, its steps run again each round for a new
ROUND, their caches kept (PM M141), by a build of shards with builder-cold's cold.patch (the
builder's own samples on its console) and give.patch (each give-back's answer, and how long
after its word it came, on the build's stderr).

- Linux: each build in a scope of the user's manager (systemd-run --user --scope) that swaps
  nothing (MemorySwapMax=0). Held, its MemoryHigh is the most a held arm of the build held
  that its host may take back from its processes alone, as the daemon and the build count
  it (daemon::budget `charged_room`: its anonymous, shared, kernel and socket memory, the
  file pages its processes map that are charged to it, and the inactive pages its kernel
  keeps beside those, `lru_buffer`), less the most its builder held cold of its files in that
  arm (inactive_file, the builder's own samples): short of the build's peak by what its
  builder holds cold, so that it fits only where its builder gives that back. Each from a
  round 0 arm, the same for rounds 1 on: round 0 runs a held arm under the host's whole
  memory, which places the writer as any budget does (D188, build/outside.rs) and presses
  nothing, then a settling arm under that arm's peak, whose own peak and cold set the
  budget (where its host's kernel throttled it, so that its peak is not its own, the first
  arm's). Not that first arm's own: told its host's whole memory as its room, a builder
  plugs and holds more than one told a budget's, and a budget from it, the most of every
  held arm since, was 4 to 49 MiB past each later round's peak on four of CI's runners
  (d9c42120, runs 42496009314 and 42496014878), their apt builds never pressed. Not from
  arms the budget pressed: their peaks are the budget's, and each budget from them is
  short by their builders' cold again. Not a free arm's: a build under a budget hands its
  own writes to a writer outside it, and a free arm's peak, which holds them, pressed
  nothing (c3ce85e, run 42366506933). Held to its anonymous peak less what its builder
  held cold (ac3e9325, run 41292692058), a build had no room for its kernel's memory nor
  the files its processes map and crawled (rust 406 to 913 s against 9 to 10 s). Not the
  scope's whole peak, memory.peak: that counts the host's page cache of the build's files
  no process maps, which the host drops at no stall (run 40426695726, 0 asks).
  A round's held arm comes after its free arm where the round is odd, before it where even.
  A round 0 first fills the builds' caches and pulls their bases with each build's free
  arm, then on Linux runs its held arm under the host's whole memory, so that no budget is
  of a free arm's peak: a first build's peak is its downloads' (npm's 688 MiB against 796
  to 819 after, which held round 2 to 340 s, b25d2d05). tables.py counts no round 0.
- Before each build, what the one before wrote is written out (sync(2)), and how long that
  took said: no arm starts under another's writeback.
- macOS, where no unprivileged process may raise the system's pressure: held, the build is
  told every 2 s that its host is short of memory (SHARDS_BUILD_PRESSURE's FIFO), as its
  watcher hears the system's pressure again every 2 s while it stays raised.

Beside it, each second, the builder's VM process on this host (phys_footprint on macOS, Pss
and RssAnon on Linux) and, on Linux, the scope's memory.current and memory.peak, and the
memory of the writer a held build starts outside its budget (D188), where it runs: whether
it ran, and so where the build's own writes were charged, is the build line's
`writer_seen`; and what the VM process, the build's own process and its writer each read
from storage and dirtied of its page cache so far (/proc/PID/io), which tell a step's
changes to its caches (the VM process's) from their merges (the build's). From the build's
output (give.patch), each phase's end (`phase`), each step's changes to a cache made durable
(`keep`) and each merge of a cache's layers (`merge`), stamped as read. Into
OUT/out.txt: a header, then `guest`, `host`, `give`, `phase`, `keep`, `merge` and `build`
lines, KEY=VALUE each, for
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


def io_of(pid, who):
    """What process `pid` read from and dirtied for its storage so far (Linux), as
    `WHO_read_bytes` and `WHO_write_bytes` fields: /proc/PID/io's read_bytes, the bytes it
    had fetched from storage, and write_bytes, those of the page cache it dirtied, each page
    counted to the task that dirtied it (mm/page-writeback.c `folio_account_dirtied`,
    `task_io_account_write`; Documentation/filesystems/proc.rst), whichever cgroup the page
    is charged to; none where it cannot be read."""
    try:
        got = dict(l.split(": ", 1) for l in open(f"/proc/{pid}/io").read().splitlines() if ": " in l)
        return f"{who}_read_bytes={int(got['read_bytes'])} {who}_write_bytes={int(got['write_bytes'])}"
    except (OSError, KeyError, ValueError):
        return None


def majflt(pid):
    """The major faults process `pid` has taken so far (Linux, /proc/PID/stat's `majflt`, its
    12th field: faults that read a page from storage), none where it has gone."""
    try:
        stat = open(f"/proc/{pid}/stat").read()
        return int(stat[stat.rindex(")") + 2:].split()[9])
    except (OSError, ValueError, IndexError):
        return None


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


def lru_buffer(mapped):
    """The page cache a cgroup's kernel keeps inactive beside `mapped` bytes its processes
    map before it takes those (mm/vmscan.c `inactive_is_low`; daemon::budget `lru_buffer`)."""
    gb = (mapped * 2) >> 30
    ratio = 1 if gb == 0 else max(int((gb * 10) ** 0.5), 1)
    return mapped // ratio


def mapped_files(scope):
    """The bytes of files the processes of cgroup `scope` and those below it map, each
    page once among them, wherever its page is charged (their smaps_rollup's `Pss_File`;
    daemon::budget `mapped_under`)."""
    total = 0
    for d, _, _ in os.walk(scope):
        try:
            pids = open(os.path.join(d, "cgroup.procs")).read().split()
        except OSError:
            continue
        for pid in pids:
            try:
                for line in open(f"/proc/{pid}/smaps_rollup"):
                    w = line.split()
                    if w and w[0] == "Pss_File:":
                        total += int(w[1]) << 10
            except (OSError, ValueError):
                pass
    return total


def holding(scope):
    """What cgroup `scope` holds that its host may take back from its processes alone, as
    a build counts it (daemon::budget `charged_room`: `Holding::held` of the files its
    processes map only those charged to it, memory.stat's `file_mapped`), with the inactive
    pages its kernel keeps beside them (`lru_buffer`), and memory.stat's fields read for it,
    with `pss_file` the files they map wherever charged (`mapped_files`); none where it
    cannot be read."""
    files = mapped_files(scope)
    current = int(open(f"{scope}/memory.current").read())
    stat = {k: int(v) for k, v in (l.split() for l in open(f"{scope}/memory.stat") if len(l.split()) == 2)}
    get = lambda k: stat.get(k, 0)
    kernel = stat.get("kernel", sum(get(k) for k in ("kernel_stack", "pagetables", "sec_pagetables",
                                                     "percpu", "vmalloc", "slab")))
    own = kernel + sum(get(k) for k in ("anon", "shmem", "sock", "zswap"))
    mapped = get("file_mapped")
    floor = current - max(get("file") - mapped - get("shmem"), 0)
    held = min(max(own + mapped, floor), current) + lru_buffer(mapped)
    stat["pss_file"] = files
    return held, current, stat


def writer_of(scope, build_pid):
    """The cgroup of the writer a build held to a budget starts outside it (D188,
    build/outside.rs): its user's manager's scope `shards-build-writer-PID.scope` in its
    application slice, PID the build's (systemd-run --scope execs it); none where `scope` is
    not under a user's manager."""
    parts = scope.split("/")
    at = next((i for i, p in enumerate(parts) if p.startswith("user@") and p.endswith(".service")), None)
    if at is None:
        return None
    return "/".join(parts[:at + 1] + ["app.slice", f"shards-build-writer-{build_pid}.scope"])


def sample_host(build_pid, tag, stop, peak, cold, writer, throttled):
    """The builder's VM process's memory each second, and its scope's (Linux), until `stop`;
    in `peak` the most anonymous memory the scope held, the most it held that its host may
    take back from its processes alone (`holding`), and the most of that less what the
    builder held cold of its files then (`cold`'s second, its last sample's); in `writer`,
    how many samples found the build's writer outside its budget (its cgroup) and the most
    file pages and dirty pages charged to it, which say where the build's own writes went:
    a writer that never ran (CI at 94838b88, its command refused) left them under the budget,
    and its held builds slower in the steps that write (PM M442). The build ended where the
    room runs short."""
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
            got = None if macos else majflt(vm)
            if got is not None:
                fields.append(f"vm_majflt={got}")
            got = None if macos else io_of(vm, "vm")
            if got:
                fields.append(got)
        # The build's own process, which writes a cache's merges and makes its files durable,
        # apart from its VM process, which writes a step's changes to its caches (D134).
        got = None if macos else io_of(build_pid, "build")
        if got:
            fields.append(got)
        scope = None if macos else scope_of(build_pid)
        if scope:
            try:
                held, current, stat = holding(scope)
                anon = stat.get("anon", 0)
                need = max(held - cold[1], 0)
                peak[0] = max(peak[0], anon)
                peak[1] = max(peak[1], held)
                peak[2] = max(peak[2], need)
                fields.append(f"scope_current={current} scope_held={held} scope_need={need} " + " ".join(
                    f"scope_{k}={stat.get(k, 0)}" for k in ("anon", "file", "file_mapped", "file_dirty",
                                                           "file_writeback", "shmem", "kernel", "pss_file")))
            except (OSError, ValueError):
                pass
            try:
                events = dict(l.split() for l in open(f"{scope}/memory.events") if len(l.split()) == 2)
                throttled[0] = max(throttled[0], int(events.get("high", 0)) + int(events.get("max", 0)))
                fields.append(f"scope_high_events={events.get('high', 0)} scope_max_events={events.get('max', 0)}")
            except (OSError, ValueError):
                pass
            outside = writer_of(scope, build_pid)
            try:
                if outside:
                    stat = {k: int(v) for k, v in (l.split() for l in open(f"{outside}/memory.stat")
                                                   if len(l.split()) == 2)}
                    current = int(open(f"{outside}/memory.current").read())
                    writer["seen"] += 1
                    writer["file"] = max(writer["file"], stat.get("file", 0))
                    writer["dirty"] = max(writer["dirty"], stat.get("file_dirty", 0))
                    fields.append(f"writer_current={current} " + " ".join(
                        f"writer_{k}={stat.get(k, 0)}" for k in ("file", "file_dirty", "file_writeback")))
                    for pid in open(f"{outside}/cgroup.procs").read().split()[:1]:
                        got = io_of(pid, "writer")
                        if got:
                            fields.append(got)
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
# The build's own (give.patch): each phase's end (`crate::phase`), each step's changes to a
# cache made durable, each merge of a cache's layers.
OWN = re.compile(r"builder-(phase|keep|merge|room) (.*)$")
STEP = re.compile(r"^#(\d+) \[")
DONE = re.compile(r"^#(\d+) DONE (\d+(?:\.\d+)?)s")


def tail(path, tag, stop, now_step, held_cold):
    """The builder's console as it is written, each whole line stamped as it is read, until
    `stop` and the file's end; what it holds cold of its files (inactive_file) now and the
    most it held, in `held_cold`."""
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
                    held_cold[1] = int(pairs.get("inactive_file", 0))
                    held_cold[0] = max(held_cold[0], held_cold[1])
                except ValueError:
                    pass
                fields = " ".join(f"{k}={v}" for k, v in pairs.items())
                say(f"guest t={now:.3f} {tag} step={now_step[0]} step_end={int(end)} {fields}")


def one(r, name, arm, budget):
    """A build of `name` in round `r`, `arm` free or held (to `budget` bytes on Linux): the
    most its scope held that its host may take back from its processes alone, the most its
    builder held cold of its files, and whether its host's kernel throttled the scope past
    its budget (its memory.events' `high` and `max`)."""
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
    # What the build before wrote reaches its disk first: a held arm after a free one
    # otherwise began under the free arm's writeback, its base unpacked as 700 MiB of the
    # free arm's pages went out (CI, 573e4bf8), where a free arm never follows its own.
    synced = time.time()
    os.sync()
    synced = time.time() - synced
    started = time.time()
    raw = open(os.path.join(out, "raw", f"{r}-{name}-{arm}.txt"), "w")
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                            errors="replace")
    stop = threading.Event()
    step, peak, held_cold, took = ["-"], [0, 0, 0], [0, 0], {}
    writer = {"seen": 0, "file": 0, "dirty": 0}
    throttled = [0]
    threads = [threading.Thread(target=sample_host, args=(proc.pid, tag, stop, peak, held_cold, writer, throttled)),
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
        m = DONE.match(line)
        if m:
            took[m.group(1)] = m.group(2)
        m = GIVE.search(line)
        if m:
            words = m.group(1).split()
            fields = " ".join(f"{k}={v}" for k, v in zip(words[::2], words[1::2]))
            say(f"give t={now:.3f} {tag} {fields}")
        m = OWN.search(line)
        if m:
            words = m.group(2).split()
            fields = " ".join(f"{k}={v}" for k, v in zip(words[::2], words[1::2]))
            say(f"{m.group(1)} t={now:.3f} {tag} step={step[0]} {fields}")
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
    say(f"build {tag} exit={code} s={time.time() - started:.1f} budget={budget} scope_anon_peak={peak[0]} "
        f"scope_held_peak={peak[1]} scope_need_peak={peak[2]} held_cold={held_cold[0]} sync_s={synced:.1f} "
        f"writer_seen={writer['seen']} writer_file_peak={writer['file']} writer_dirty_peak={writer['dirty']} "
        f"scope_throttled={throttled[0]} "
        f"steps={','.join(f'{k}:{v}' for k, v in took.items()) or '-'}")
    for args in (["rmi", "-f", f"builder-pressure-{name}"], ["image", "prune", "-f"]):
        subprocess.run([shards, *args], env=env, capture_output=True)
    return peak[1], held_cold[0], throttled[0] > 0


header()
builds = a.builds.split(",")
# A budget that holds nothing back, the host's memory: its build places its writer outside it
# as under any budget (build/outside.rs `budgeted_at`), and is pressed by none.
WHOLE = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
seen = {}
for name in builds:
    for arm in ("free",) if macos else ("free", "held", "settle"):
        if short.is_set() or room() < ROOM:
            say(f"room {room() >> 20} MiB: ending the run")
            sys.exit(1)
        if arm == "free":
            one(0, name, arm, 0)
        elif arm == "held":
            whole = one(0, name, arm, WHOLE)
        else:
            # Under a budget of the peak the build held under the host's whole memory: a
            # builder told its host's whole memory as its room holds more than one told a
            # budget's (it plugs, and reports, as that room lets it), so that peak is no held
            # arm's, and the settling arm under it is pressed by none where its own is less.
            held, cold, pressed = one(0, name, arm, whole[0])
            seen[name] = (held if not pressed else whole[0]) - (cold if not pressed else whole[1])
for r in range(1, a.rounds + 1):
    order = builds[(r - 1) % len(builds):] + builds[:(r - 1) % len(builds)]
    for name in order:
        arms = ("free", "held") if r % 2 == 1 else ("held", "free")
        for arm in arms:
            if short.is_set() or room() < ROOM:
                say(f"room {room() >> 20} MiB: ending the run")
                sys.exit(1)
            budget = max(seen.get(name, 0), 0) if arm == "held" and not macos else 0
            one(r, name, arm, budget)
say("done")
