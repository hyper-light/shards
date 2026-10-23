#!/usr/bin/env python3
"""What a microVM held to a budget pays to read a file through its volume's DAX window
(D188, D189): under a slice of the user's systemd manager whose MemoryHigh is past the
microVM idle and short of the file, `dd` of a 1 GiB file from a volume, its share's window
as long as the host's memory (given) and with none (SHARDS_SHARE_WINDOW_MIB=0: FUSE,
through the guest's page cache), the file sparse and written out; and under no budget
(MemoryHigh=infinity), what the window's reads cost the microVM by themselves. Rounds
interleave the cases, each first in turn; the file's pages are out of the host's page cache
before each read, so that the read's microVM brings them in and pays for them.

    measure.py SHARDS OUT [--huge SHARDS] [--fuse NAME=SHARDS]... [--arms A,B]
               [--guest-mib N] [--rounds N] [--highs 384M,infinity] [--size-mib 1024]
               [--perf] [--stop-slice] [--parent-slice NAME]

With --huge, a third arm: that build of shards (huge.patch: each window range on huge
pages, MADV_HUGEPAGE), its window as long as the host's memory. With --stop-slice, the
slice is stopped after each read, as its user may stop it, which ends the spawner and warm
microVMs each daemon keeps there: what the runs after it meet. Each --fuse NAME=SHARDS is
an arm of another build of shards, through FUSE (no window); --arms picks the arms run, by
name. With --guest-mib, the reader's run is held to that many MiB in its guest
(`--memory`, no swap, `--oom-kill-disable`: its memory.high), its budget its own. With
--parent-slice NAME, the slice is made in NAME.slice, which a shared host may hold to a
`MemoryMax` of its own: its reads under no budget of their own are then held to that one.

Prints n, p50, p90, p99 and max of each read's seconds, with host, OS and revision. Into
OUT, for each read, what its slice held each second (memory.current, memory.stat's file,
anon and reclaim counters, memory.events, memory.pressure), its VM process's vCPU threads'
states and CPU times, and KVM's counters of its VM where `sudo -n` reads debugfs; with
--perf, a profile of the whole host 5 to 15 s into each read (`sudo -n perf`).

Into OUT/<case>.vms, what each VM process in the slice holds as the reader starts and once
its read is done, mapping by mapping, and what its guest says of its own memory; into
OUT/<case>.guest, from within the reader's guest each second of its read, where its `dd`
waits (five looks at its wchan), its run's memory and reclaim counters, its kernel's
memory and its reclaim; into OUT/daemon-<arm>.timed.log, each line of the arm's daemon log
with the time it was seen.

Linux, with KVM and a user's systemd manager (XDG_RUNTIME_DIR's bus). Each arm has a home
of its own, which pulls alpine once, and an empty DOCKER_CONFIG; all of it is removed, and
its daemons and the slice stopped, however the run ends.
"""
import argparse, atexit, os, platform, shutil, signal, subprocess, sys, tempfile, threading, time

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("out")
p.add_argument("--rounds", type=int, default=3)
p.add_argument("--highs", default="384M,infinity")
p.add_argument("--size-mib", type=int, default=1024)
p.add_argument("--perf", action="store_true")
p.add_argument("--huge")
p.add_argument("--fuse", action="append", default=[], metavar="NAME=SHARDS")
p.add_argument("--arms")
p.add_argument("--guest-mib", type=int)
p.add_argument("--stop-slice", action="store_true")
p.add_argument("--parent-slice")
a = p.parse_args()

shards = os.path.realpath(a.shards)
out = os.path.realpath(a.out)
os.makedirs(out, exist_ok=True)
work = tempfile.mkdtemp(prefix="dax-budget-")
config, vol = os.path.join(work, "docker-config"), os.path.join(work, "vol")
os.makedirs(config)
os.makedirs(vol)
# Under --parent-slice, the slice is its child (systemd.slice(5): a dash nests a slice in the
# one its name begins with), which a shared host caps.
PARENT = f"{a.parent_slice}.slice/" if a.parent_slice else ""
SLICE = f"{a.parent_slice}-shardsdax{os.getpid()}.slice" if a.parent_slice else f"shardsdax{os.getpid()}.slice"
uid = os.getuid()
CGROUP = f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{PARENT}{SLICE}"
# Each arm's build of shards and what its daemon is given: the window arms a window as long
# as the host's memory, given, as the daemon's default is none on a virtual machine on an
# AMD CPU (D189, PM M308).
HOST_MIB = (os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") >> 20) & ~1
WINDOW = {"SHARDS_SHARE_WINDOW_MIB": str(HOST_MIB)}
ARMS = {"window": (shards, WINDOW), "none": (shards, {"SHARDS_SHARE_WINDOW_MIB": "0"})}
if a.huge:
    ARMS["huge"] = (os.path.realpath(a.huge), WINDOW)
for given in a.fuse:
    arm, _, build = given.partition("=")
    ARMS[arm] = (os.path.realpath(build), {"SHARDS_SHARE_WINDOW_MIB": "0"})
if a.arms:
    ARMS = {arm: ARMS[arm] for arm in a.arms.split(",")}
FILES = ("sparse", "written")
homes = {arm: os.path.join(work, f"home-{arm}") for arm in ARMS}


def env(arm):
    e = dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config)
    e.pop("SHARDS_SHARE_WINDOW_MIB", None)
    e.update(ARMS[arm][1])
    return e


def run(arm, *args, check=True, timeout=None):
    done = subprocess.run([ARMS[arm][0], *args], env=env(arm), capture_output=True, text=True, timeout=timeout)
    if check and done.returncode != 0:
        raise SystemExit(f"{arm}: shards {' '.join(args)}: exit {done.returncode}\n{done.stdout}{done.stderr}")
    return done


def systemctl(*args):
    return subprocess.run(["systemctl", "--user", *args], capture_output=True, text=True)


@atexit.register
def clean():
    """However the run ends, a failed step's exit included: each arm's daemon stopped, its
    runs with it, and its log kept in OUT; the slice stopped; this run's own files removed."""
    for arm in ARMS:
        subprocess.run([ARMS[arm][0], "daemon", "stop"], env=env(arm), capture_output=True)
        try:
            shutil.copy(os.path.join(homes[arm], "daemon.log"), os.path.join(out, f"daemon-{arm}.log"))
        except OSError:
            pass
    systemctl("revert", SLICE)
    systemctl("stop", SLICE)
    shutil.rmtree(work, ignore_errors=True)


# Ended from outside (a CI job cancelled at its time limit): ended as from within, so that
# `clean` keeps the daemons' logs.
signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))


def drop(path):
    """The file's pages out of the host's page cache (clean, unmapped once no microVM
    maps it)."""
    fd = os.open(path, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


# What the reader's guest says of its read each second: where its dd waits, five looks a
# fifth of a second apart, its state and what it has read; its workload's cgroup's memory,
# events and reclaim; its kernel's memory; and its kernel's reclaim.
GUEST = (
    "while :; do t=$(cut -d' ' -f1 /proc/uptime); p=$(pidof dd); w=; "
    "for _ in 1 2 3 4 5; do w=\"$w $(cat /proc/$p/wchan 2>/dev/null || echo -)\"; sleep 0.2; done; "
    "echo \"$t dd$w $(sed -n 's/^State:[[:space:]]*\\([A-Z]\\).*/\\1/p' /proc/$p/status 2>/dev/null) "
    "$(sed -n 's/^rchar: //p' /proc/$p/io 2>/dev/null) | current $(cat /sys/fs/cgroup/memory.current) "
    "$(tr '\\n' ' ' < /sys/fs/cgroup/memory.events)$(grep -E '^(anon|file|kernel|pgscan|pgsteal|workingset_refault_file) ' "
    "/sys/fs/cgroup/memory.stat | tr '\\n' ' ')| $(grep -E '^(MemTotal|MemFree|Cached):' /proc/meminfo | tr -s ' ' | tr '\\n' ' ')"
    "| $(grep -E '^(pgscan_kswapd|pgscan_direct|pgsteal_kswapd|pgsteal_direct|workingset_refault_file|allocstall_movable) ' "
    "/proc/vmstat | tr '\\n' ' ')\"; done"
)


def guest_sample(arm, path, stop):
    """What the reader's guest says each second (GUEST) until `stop`, each line with the
    host's time it came."""
    with open(path, "w") as f:
        p = subprocess.Popen([ARMS[arm][0], "exec", "reader", "sh", "-c", GUEST], env=env(arm),
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        stopper = threading.Thread(target=lambda: (stop.wait(), p.kill()))
        stopper.start()
        for line in p.stdout:
            f.write(f"{time.time():.3f} {line}")
            f.flush()
        p.wait()
        stopper.join()


def timed_log(arm, stop):
    """Each line of the arm's daemon log, with the host's time it was seen, into
    OUT/daemon-<arm>.timed.log, until `stop`."""
    log, seen = os.path.join(homes[arm], "daemon.log"), 0
    with open(os.path.join(out, f"daemon-{arm}.timed.log"), "w") as f:
        while not stop.wait(0.2):
            try:
                with open(log, "rb") as d:
                    d.seek(seen)
                    new = d.read()
            except OSError:
                continue
            lines = new.split(b"\n")
            whole = b"\n".join(lines[:-1])
            if lines[:-1]:
                now = time.time()
                for line in lines[:-1]:
                    f.write(f"{now:.3f} {line.decode(errors='replace')}\n")
                f.flush()
                seen += len(whole) + 1


def vm_pids():
    """The VM processes in the slice."""
    pids = []
    for root, _, files in os.walk(CGROUP):
        if "cgroup.procs" in files:
            with open(os.path.join(root, "cgroup.procs")) as f:
                for pid in f.read().split():
                    try:
                        with open(f"/proc/{pid}/comm") as c:
                            if c.read().strip() == "shards-vm":
                                pids.append(pid)
                    except OSError:
                        pass
    return pids


def threads(pid):
    """Each vCPU thread's name, state, and user and system CPU ticks."""
    said = []
    for tid in sorted(os.listdir(f"/proc/{pid}/task")):
        try:
            with open(f"/proc/{pid}/task/{tid}/stat") as f:
                stat = f.read()
        except OSError:
            continue
        name = stat[stat.index("(") + 1 : stat.rindex(")")]
        if name.startswith("vcpu"):
            fields = stat[stat.rindex(")") + 2 :].split()
            said.append(f"{name} {fields[0]} {fields[11]} {fields[12]}")
    return " ".join(said)


def kvm(pid):
    """KVM's counters of the VM of process `pid`, where `sudo -n` reads debugfs."""
    script = (f"for d in /sys/kernel/debug/kvm/{pid}-*; do for f in $d/*; do "
              "[ -f $f ] && printf '%s %s ' ${f##*/} $(cat $f); done; done")
    done = subprocess.run(["sudo", "-n", "sh", "-c", script], capture_output=True, text=True)
    return done.stdout.strip()


def held(f, when, arm):
    """What each VM process in the slice holds, mapping by mapping (/proc/PID/smaps: each
    with a MiB or more resident, its Rss, Anonymous, Private_Dirty, Shared_Clean and
    Private_Clean in KiB), with its status's RssAnon, RssFile and RssShmem; and the reader's
    guest's view of its memory (/proc/meminfo, its run's memory.stat)."""
    f.write(f"== {when} {time.time():.3f}\n")
    for pid in vm_pids():
        try:
            with open(f"/proc/{pid}/status") as s:
                rss = " ".join(l.replace("\t", "").replace(" kB", "").replace(":", "").strip()
                               for l in s if l.startswith(("RssAnon", "RssFile", "RssShmem")))
            f.write(f"vm {pid} {rss}\n")
            with open(f"/proc/{pid}/smaps") as s:
                head, fields = None, {}
                for line in list(s) + ["0-0 end\n"]:
                    words = line.split()
                    if not words:
                        continue
                    if "-" in words[0] and not words[0].endswith(":"):
                        if head and int(fields.get("Rss", 0)) >= 1024:
                            f.write("  " + head + " " + " ".join(
                                f"{k} {fields.get(k, 0)}" for k in
                                ("Rss", "Anonymous", "Private_Dirty", "Shared_Clean", "Private_Clean")) + "\n")
                        start, end = (int(x, 16) for x in words[0].split("-"))
                        name = words[5] if len(words) > 5 else "[anon]"
                        head, fields = f"{words[0]} {(end - start) >> 20}MiB {words[1]} {name}", {}
                    elif words[0].endswith(":") and len(words) > 1:
                        fields[words[0][:-1]] = words[1]
        except OSError as e:
            f.write(f"vm {pid} {e}\n")
    try:
        guest = run(arm, "exec", "reader", "sh", "-c",
                    "grep -E '^(MemTotal|MemFree|MemAvailable|Cached|AnonPages|Shmem|Slab):' /proc/meminfo; "
                    "grep -E '^(anon|file|kernel|slab) ' /sys/fs/cgroup/memory.stat", check=False, timeout=60)
        f.write("guest " + " ".join(guest.stdout.split()) + "\n")
    except subprocess.TimeoutExpired:
        f.write("guest: no answer in 60 s\n")
    f.flush()


STAT = ("anon", "file", "file_mapped", "shmem", "pagetables", "sec_pagetables", "pgscan", "pgsteal", "pgfault",
        "pgmajfault", "workingset_refault_file", "workingset_activate_file", "active_file", "inactive_file")


def sample(path, stop):
    """What the slice holds, each second until `stop`."""
    def read(name):
        with open(os.path.join(CGROUP, name)) as f:
            return f.read()

    with open(path, "w") as f:
        while not stop.wait(1):
            try:
                stat = dict(line.split() for line in read("memory.stat").splitlines())
                lines = [
                    f"== {time.time():.3f} current {read('memory.current').strip()} high {read('memory.high').strip()}",
                    " ".join(f"{k} {stat.get(k)}" for k in STAT),
                    "events " + read("memory.events").replace("\n", " "),
                    "pressure " + read("memory.pressure").splitlines()[0],
                ]
                for pid in vm_pids():
                    lines.append(f"vm {pid} {threads(pid)}")
                    lines.append(f"kvm {kvm(pid)}")
            except (OSError, ValueError, IndexError) as e:
                lines = [f"== {time.time():.3f} {e}"]
            f.write("\n".join(lines) + "\n")
            f.flush()


def profile(name, stop):
    """The whole host's profile from 5 to 15 s into a read, unless it ends first."""
    if stop.wait(5):
        return
    data = os.path.join(out, f"{name}.perf")
    subprocess.run(["sudo", "-n", "perf", "record", "-a", "-g", "-o", data, "--", "sleep", "10"],
                   capture_output=True)
    # Reported as root while root owns its data, which perf reads of no other owner; then
    # the data kept as the run's.
    with open(os.path.join(out, f"{name}.perf.txt"), "w") as f:
        subprocess.run(["sudo", "-n", "perf", "report", "-i", data, "--stdio", "--no-children",
                        "--percent-limit", "0.5"], stdout=f, stderr=subprocess.DEVNULL)
    subprocess.run(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", data], capture_output=True)


def one(arm, file, high, name):
    """One read's seconds: a microVM started in the slice, its budget set, the file read."""
    if not os.path.isdir(homes[arm]):
        run(arm, "pull", "-q", "alpine")
    held_to = []
    if a.guest_mib:
        held_to = ["--memory", f"{a.guest_mib}m", "--memory-swap", f"{a.guest_mib}m", "--oom-kill-disable"]
    run(arm, "run", "-d", "--name", "reader", "--cgroup-parent", SLICE, *held_to, "-v", f"{vol}:/vol", "alpine",
        "sleep", "100000")
    vms = open(os.path.join(out, f"{name}.vms"), "w")
    held(vms, "started", arm)
    # Started before its budget is set, so that what it holds is in what the guest uses as
    # it is told its room.
    guest_stop = threading.Event()
    guest = threading.Thread(target=guest_sample, args=(arm, os.path.join(out, f"{name}.guest"), guest_stop))
    guest.start()
    set_ = systemctl("set-property", "--runtime", SLICE, f"MemoryHigh={high}", "MemorySwapMax=0")
    if set_.returncode != 0:
        raise SystemExit(f"the slice's budget: {set_.stderr}")
    for each in FILES:
        drop(os.path.join(vol, each))
    stop = threading.Event()
    watchers = [threading.Thread(target=sample, args=(os.path.join(out, f"{name}.txt"), stop))]
    if a.perf:
        watchers.append(threading.Thread(target=profile, args=(name, stop)))
    for w in watchers:
        w.start()
    start = time.perf_counter()
    done = run(arm, "exec", "reader", "dd", f"if=/vol/{file}", "of=/dev/null", "bs=1M", check=False, timeout=1800)
    took = time.perf_counter() - start
    stop.set()
    guest_stop.set()
    for w in watchers + [guest]:
        w.join()
    held(vms, "read", arm)
    vms.close()
    # The slice stays, its budget taken off, unless asked: stopping it ends the warm
    # microVMs the daemons keep in it for their next runs.
    run(arm, "rm", "-f", "reader", check=False)
    if a.stop_slice:
        systemctl("revert", SLICE)
        systemctl("stop", SLICE)
    else:
        systemctl("set-property", "--runtime", SLICE, "MemoryHigh=infinity")
    if done.returncode != 0:
        raise SystemExit(f"{name}: exit {done.returncode}\n{done.stdout}{done.stderr}")
    return took


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


size = a.size_mib << 20
with open(os.path.join(vol, "sparse"), "wb") as f:
    f.truncate(size)
with open(os.path.join(vol, "written"), "wb") as f:
    block = b"\x5a" * (1 << 20)
    for _ in range(a.size_mib):
        f.write(block)
    f.flush()
    os.fsync(f.fileno())

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
try:
    with open("/sys/kernel/mm/transparent_hugepage/enabled") as f:
        thp = f.read().strip()
except OSError:
    thp = "none"
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, THP {thp}, "
      f"{os.sysconf('SC_PAGE_SIZE') * os.sysconf('SC_PHYS_PAGES') >> 20} MiB, {os.cpu_count()} CPUs, "
      f"revision {rev}; file {a.size_mib} MiB")
logs_stop = threading.Event()
logs = [threading.Thread(target=timed_log, args=(arm, logs_stop), daemon=True) for arm in ARMS]
for t in logs:
    t.start()
cases = [(high, arm, file) for high in a.highs.split(",") for file in FILES for arm in ARMS]
times = {case: [] for case in cases}
for i in range(a.rounds):
    for high, arm, file in cases[i % len(cases):] + cases[: i % len(cases)]:
        took = one(arm, file, high, f"{high}-{arm}-{file}-{i + 1}")
        times[(high, arm, file)].append(took)
        print(f"round {i + 1} {high} {arm} {file}: {took:.2f} s", file=sys.stderr, flush=True)
for (high, arm, file), xs in times.items():
    print(f"high {high:<8} {arm:<6} {file:<7} n {len(xs)} p50 {pct(xs, .5):8.2f} p90 {pct(xs, .9):8.2f} "
          f"p99 {pct(xs, .99):8.2f} max {max(xs):8.2f} s")
