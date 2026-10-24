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
               [--guest-mib N] [--rounds N] [--highs 384M,short,infinity] [--size-mib 1024]
               [--pool N] [--perf] [--stop-slice] [--parent-slice NAME] [--files sparse,written]
               [--as-e2e [--reads N]]

With --huge, a third arm: that build of shards (huge.patch: each window range on huge
pages, MADV_HUGEPAGE), its window as long as the host's memory. With --stop-slice, the
slice is stopped after each read, as its user may stop it, which ends the spawner and warm
microVMs each daemon keeps there: what the runs after it meet. Each --fuse NAME=SHARDS is
an arm of another build of shards, through FUSE (no window); --arms picks the arms run, by
name. With --guest-mib, the reader's run is held to that many MiB in its guest
(`--memory`, no swap, `--oom-kill-disable`: its memory.high), its budget its own. With
--parent-slice NAME, the slice is made in NAME.slice, which a shared host may hold to a
`MemoryMax` of its own: its reads under no budget of their own are then held to that one.
A budget of `short` is set as each read's run has started and the slice's statistics have
caught up with its counter: a mebibyte short of what its microVMs hold and the inactive
pages its kernel keeps beside their mapped ones (as the daemon counts a budget's room,
daemon/budget.rs), so that the run is held within the least room a run goes on in; with
`--pool 0`, no warm microVM is left to end for room, and it is held so for its whole read.
Each read under it is said held short throughout, or not, by its daemon's log. With
--pool N, each arm's daemon keeps a pool of N (SHARDS_POOL). --files picks the files read.

With --as-e2e, each round is the FUSE budget E2E's phases
(a_microvm_held_to_a_budget_reads_through_fuse_within_twice_its_time_without_one) for each
arm in turn, in its order, in a home of its own given the images and guest kernel pulled
once, and a slice made for it: a first run, which saves the image's template under the
slice and warms its pool there; --reads reads under no budget (`infinity`); --reads under a
budget the slice's microVMs leave 128 MiB of (`past128`); one under that budget set once its
run has started (`late`), whose VM process is waited out; and --reads under a budget short
of the slice's microVMs (`short`), set once for them. Each read but the late one is a run of
its own, taken from the pool, its seconds its dd's, and the file stays in the host's page
cache between reads, as the E2E's do; --highs is not read. A short budget held each run
within the least room a run goes on in on CI, where a pool of none (`--pool 0`) held the
reader's own memory a mebibyte past its budget, which its host throttled: its reads took
24 minutes, the E2E's 13 s.

Prints n, p50, p90, p99 and max of each read's seconds, with host, OS and revision. Into
OUT, for each read, what its slice held each second (memory.current, memory.stat's file,
anon and reclaim counters, memory.events, memory.pressure), its VM process's vCPU threads'
states and CPU times, and KVM's counters of its VM where `sudo -n` reads debugfs; with
--perf, a profile of the whole host 5 to 15 s into each read (`sudo -n perf`).

Into OUT/<case>.txt each second, too, each thread of each VM process in the slice, its
state and, not running, where it waits (its wchan). Into OUT/<case>.vms, what each VM
process in the slice holds as the reader starts and once its read is done, mapping by
mapping, and what its guest says of its own memory; into
OUT/<case>.guest, from within the reader's guest each second of its read, where its `dd`
waits (five looks at its wchan), its run's memory and reclaim counters, its kernel's
memory and its reclaim; into OUT/daemon-<arm>.timed.log, each line of the arm's daemon log
with the time it was seen.

Linux, with KVM and a user's systemd manager (XDG_RUNTIME_DIR's bus). Each arm has a home
of its own, which pulls alpine once, and an empty DOCKER_CONFIG; all of it is removed, and
its daemons and the slice stopped, however the run ends.
"""
import argparse, atexit, glob, os, platform, re, shutil, signal, subprocess, sys, tempfile, threading, time

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("out")
p.add_argument("--rounds", type=int, default=3)
p.add_argument("--highs", default="384M,infinity")
p.add_argument("--pool", type=int)
p.add_argument("--size-mib", type=int, default=1024)
p.add_argument("--perf", action="store_true")
p.add_argument("--huge")
p.add_argument("--fuse", action="append", default=[], metavar="NAME=SHARDS")
p.add_argument("--arms")
p.add_argument("--guest-mib", type=int)
p.add_argument("--stop-slice", action="store_true")
p.add_argument("--parent-slice")
p.add_argument("--files", default="sparse,written")
p.add_argument("--as-e2e", action="store_true")
p.add_argument("--reads", type=int, default=3)
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
FILES = tuple(a.files.split(","))
homes = {arm: os.path.join(work, f"home-{arm}") for arm in ARMS}
# Under --as-e2e, the home the images and guest kernel are pulled into once, which each
# round's homes are given.
SEED = os.path.join(work, "seed")


def env(arm, extra=None):
    e = dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config)
    e.pop("SHARDS_SHARE_WINDOW_MIB", None)
    e.update(ARMS[arm][1])
    if a.pool is not None:
        e["SHARDS_POOL"] = str(a.pool)
    e.update(extra or {})
    return e


def run(arm, *args, check=True, timeout=None, extra=None):
    done = subprocess.run([ARMS[arm][0], *args], env=env(arm, extra), capture_output=True, text=True,
                          timeout=timeout)
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


# GUEST's samples, of the shell's builtins but one `sleep` a second, for runs held within a
# room of a few MiB, where GUEST's dozen processes a second took the run's room from its read
# (--as-e2e): where its dd waits, one look; its run's memory, events and reclaim.
GUEST_LIGHT = (
    "until p=$(pidof dd); do sleep 0.05; done; "
    "while [ -d /proc/$p ]; do read -r t _ < /proc/uptime; w=-; read -r w < /proc/$p/wchan; "
    "read -r c < /sys/fs/cgroup/memory.current; e=; while read -r k v; do e=\"$e $k $v\"; "
    "done < /sys/fs/cgroup/memory.events; m=; while read -r k v; do case $k in "
    "anon|file|kernel|pgscan|pgsteal|workingset_refault_file) m=\"$m $k $v\";; esac; "
    "done < /sys/fs/cgroup/memory.stat; echo \"$t dd ${w:--} | current $c$e$m\"; sleep 1; done"
)


def guest_sample(arm, path, stop, script=GUEST):
    """What the reader's guest says each second (`script`) until `stop`, each line with the
    host's time it came."""
    with open(path, "w") as f:
        p = subprocess.Popen([ARMS[arm][0], "exec", "reader", "sh", "-c", script], env=env(arm),
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
    OUT/daemon-<arm>.timed.log, until `stop`; the log of each home the arm is given in
    turn (--as-e2e)."""
    log, seen = None, 0
    with open(os.path.join(out, f"daemon-{arm}.timed.log"), "w") as f:
        while not stop.wait(0.2):
            if log != os.path.join(homes[arm], "daemon.log"):
                log, seen = os.path.join(homes[arm], "daemon.log"), 0
                f.write(f"{time.time():.3f} == {log}\n")
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


def every_thread(pid):
    """Each thread's name, state and, not running, where it waits: name:state:wchan."""
    said = []
    for tid in sorted(os.listdir(f"/proc/{pid}/task")):
        try:
            with open(f"/proc/{pid}/task/{tid}/stat") as f:
                stat = f.read()
            with open(f"/proc/{pid}/task/{tid}/wchan") as f:
                wchan = f.read().strip() or "-"
        except OSError:
            continue
        name = stat[stat.index("(") + 1 : stat.rindex(")")].replace(" ", "_")
        state = stat[stat.rindex(")") + 2 :].split()[0]
        said.append(f"{name}:{state}:{wchan if state != 'R' else '-'}")
    return " ".join(said)


def kvm(pid):
    """KVM's counters of the VM of process `pid`, where `sudo -n` reads debugfs."""
    script = (f"for d in /sys/kernel/debug/kvm/{pid}-*; do for f in $d/*; do "
              "[ -f $f ] && printf '%s %s ' ${f##*/} $(cat $f); done; done")
    done = subprocess.run(["sudo", "-n", "sh", "-c", script], capture_output=True, text=True)
    return done.stdout.strip()


def held(f, when, arm, guest=True):
    """What each VM process in the slice holds, mapping by mapping (/proc/PID/smaps: each
    with a MiB or more resident, its Rss, Anonymous, Private_Dirty, Shared_Clean and
    Private_Clean in KiB), with its status's RssAnon, RssFile and RssShmem; and, with
    `guest`, the reader's guest's view of its memory (/proc/meminfo, its run's
    memory.stat)."""
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
    if not guest:
        f.flush()
        return
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
                    lines.append(f"threads {pid} {every_thread(pid)}")
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


def stat_of(pid):
    """Process `pid`'s state and start time (proc_pid_stat(5) fields 3 and 22), or None
    once it is gone."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
        return fields[0], fields[19]
    except (OSError, IndexError):
        return None


def ended(pid, started, timeout=60):
    """Waits until process `pid`, started at `started`, has ended: gone, a zombie, or its
    number another's."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        now = stat_of(pid)
        if now is None or now[0] in ("Z", "X") or now[1] != started:
            return
        time.sleep(0.02)
    raise SystemExit(f"VM process {pid} still runs {timeout} s after its run was removed")


def short_budget():
    """A mebibyte short of what the slice's microVMs hold and the inactive pages their
    kernel keeps beside the mapped ones: all the slice holds but its page cache no process
    maps, and as many as the mapped under a GiB of them (daemon/budget.rs, `lru_buffer`;
    mm/vmscan.c, `inactive_is_low`); read once the slice's statistics have caught up with
    its counter, which a microVM just ended leaves them behind (PM M309)."""
    deadline = time.time() + 60
    while True:
        with open(os.path.join(CGROUP, "memory.current")) as f:
            current = int(f.read())
        with open(os.path.join(CGROUP, "memory.stat")) as f:
            stat = dict((k, int(v)) for k, v in (line.split() for line in f))
        if stat["anon"] + stat["file"] <= current:
            break
        if time.time() > deadline:
            raise SystemExit(f"the slice's statistics still count more than it holds ({current}): {stat}")
        time.sleep(0.1)
    mapped = stat["file_mapped"]
    unmapped = max(0, stat["file"] - mapped - stat["shmem"])
    gb = (mapped * 2) >> 30
    ratio = 1 if gb == 0 else max(1, int((gb * 10) ** 0.5))
    return max(0, current - unmapped + mapped // ratio - (1 << 20))


def held_short(arm, run_id):
    """What the arm's daemon log says it told run `run_id` of its budget: whether it was
    held within the least room a run goes on in from the first word of it on, and never
    told room past that; with the holds it was told."""
    try:
        with open(os.path.join(homes[arm], "daemon.log"), errors="replace") as f:
            said = [line for line in f if f"container {run_id}:" in line]
    except OSError:
        return "no daemon log"
    least, holds, roomier = None, [], []
    for line in said:
        if "the least room a run goes on in" in line:
            m = re.search(r"held within (\d+) KiB", line)
            if m:
                holds.append(int(m.group(1)))
                least = max(least or 0, int(m.group(1)))
        elif least is not None:
            m = re.search(r"has room again, (\d+) MiB|held within its budget, (\d+) MiB of room", line)
            if m:
                roomier.append(int(m.group(1) or m.group(2)))
    if least is None:
        return "never held within the least room"
    past = [r for r in roomier if r << 10 > least]
    verdict = "held short throughout" if not past else f"given room past the least: {past} MiB"
    return f"{verdict}; held within {holds} KiB"


def set_budget(high):
    """The slice's budget, MemoryHigh, with no swap."""
    set_ = systemctl("set-property", "--runtime", SLICE, f"MemoryHigh={high}", "MemorySwapMax=0")
    if set_.returncode != 0:
        raise SystemExit(f"the slice's budget: {set_.stderr}")


def slice_holds():
    """What the slice holds but its page cache no process maps, which its kernel takes back
    first, the host's of the volume's reads among it: as the E2E counts it."""
    with open(os.path.join(CGROUP, "memory.current")) as f:
        current = int(f.read())
    with open(os.path.join(CGROUP, "memory.stat")) as f:
        stat = dict((k, int(v)) for k, v in (line.split() for line in f))
    return max(0, current - max(0, stat["file"] - stat["file_mapped"] - stat["shmem"]))


def one(arm, file, high, name, budget=None, cached=False, keep=False):
    """One read's seconds: a microVM started in the slice, its budget set (`budget`, else
    `high`'s), the file read; out of the host's page cache first, unless `cached`; the
    budget taken off after, unless `keep`."""
    held_to = []
    if a.guest_mib:
        held_to = ["--memory", f"{a.guest_mib}m", "--memory-swap", f"{a.guest_mib}m", "--oom-kill-disable"]
    if not os.path.isdir(homes[arm]):
        run(arm, "pull", "-q", "alpine")
        # The image's first run boots cold and saves its template once its microVM has
        # settled, its guest's RAM all anonymous memory its host cannot take back without
        # swap: a budget a mebibyte short of it cannot be met, and its read is throttled
        # for as long as it runs (811 to 1,520 s: DAX budget runs 40076551870,
        # 40082475577). Made here, unbudgeted, by a run of the readers' shape, in the slice
        # as theirs are, kept until its template is saved, so that every read's run
        # restores the template, as a user's second run does: a run of `true` ends before
        # it settles (run 40090430319's first reads booted cold), and a template's key
        # holds its run's shares (run.rs `template`: 40093677719's, made with no volume,
        # left them booting cold too).
        run(arm, "run", "-d", "--name", "warm", "--cgroup-parent", SLICE, *held_to, "-v", f"{vol}:/vol",
            "--pull", "never", "alpine", "sleep", "600")
        pid = run(arm, "inspect", "-f", "{{.State.Pid}}", "warm").stdout.strip()
        started = stat_of(pid)
        deadline = time.time() + 300
        while not glob.glob(os.path.join(homes[arm], "templates", "*", "current")):
            if time.time() > deadline:
                raise SystemExit(f"{arm}: no template saved in 300 s")
            time.sleep(0.5)
        run(arm, "rm", "-f", "warm", check=False)
        if started:
            ended(pid, started[1])
    run_id = run(arm, "run", "-d", "--name", "reader", "--cgroup-parent", SLICE, *held_to, "-v", f"{vol}:/vol",
                 "alpine", "sleep", "100000").stdout.strip()
    pid = run(arm, "inspect", "-f", "{{.State.Pid}}", "reader").stdout.strip()
    started = stat_of(pid)
    vms = open(os.path.join(out, f"{name}.vms"), "w")
    held(vms, "started", arm)
    # Started before its budget is set, so that what it holds is in what the guest uses as
    # it is told its room.
    guest_stop = threading.Event()
    guest = threading.Thread(target=guest_sample, args=(arm, os.path.join(out, f"{name}.guest"), guest_stop))
    guest.start()
    budget = budget or (str(short_budget()) if high == "short" else high)
    set_budget(budget)
    for each in () if cached else FILES:
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
    if high == "short":
        print(f"{name}: budget {int(budget) >> 10} KiB; {held_short(arm, run_id)}", file=sys.stderr, flush=True)
    # The slice stays, its budget taken off, unless asked: stopping it ends the warm
    # microVMs the daemons keep in it for their next runs. The next read waits for this
    # one's VM process to end, which `rm -f` leaves ending once its run has, its guest's
    # memory still the slice's until then.
    run(arm, "rm", "-f", "reader", check=False)
    if started:
        ended(pid, started[1])
    if a.stop_slice:
        systemctl("revert", SLICE)
        systemctl("stop", SLICE)
    elif not keep:
        systemctl("set-property", "--runtime", SLICE, "MemoryHigh=infinity")
    if done.returncode != 0:
        raise SystemExit(f"{name}: exit {done.returncode}\n{done.stdout}{done.stderr}")
    return took


# busybox dd's last word: "N bytes (...) copied, S seconds, R".
DD_SECONDS = re.compile(r"copied, ([\d.]+) seconds")


def pooled(arm, file, high, name):
    """One read as the E2E's are (--as-e2e): a run of its own in the slice, taken from the
    pool under the slice's budget as it stands, its command the read; its seconds as its dd
    says them, or None for a run that did not start or read, which is said and its round
    goes on. Its VM process is not waited out, as `run --rm` leaves it ending."""
    started = run(arm, "run", "-d", "--name", "reader", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol",
                  "alpine", "dd", f"if=/vol/{file}", "of=/dev/null", "bs=1M", check=False)
    if started.returncode != 0:
        print(f"{name}: failed: its run did not start (exit {started.returncode}): "
              f"{(started.stderr or started.stdout).strip().splitlines()[:1]}", file=sys.stderr, flush=True)
        run(arm, "rm", "-f", "reader", check=False)
        return None
    run_id = started.stdout.strip()
    vms = open(os.path.join(out, f"{name}.vms"), "w")
    # Its guest asked nothing more than GUEST_LIGHT asks as it reads, which a look into
    # it would add a process to.
    held(vms, "started", arm, guest=False)
    stop = threading.Event()
    watchers = [threading.Thread(target=sample, args=(os.path.join(out, f"{name}.txt"), stop)),
                threading.Thread(target=guest_sample,
                                 args=(arm, os.path.join(out, f"{name}.guest"), stop, GUEST_LIGHT))]
    if a.perf:
        watchers.append(threading.Thread(target=profile, args=(name, stop)))
    for w in watchers:
        w.start()
    start = time.perf_counter()
    waited = run(arm, "wait", "reader", check=False, timeout=1800)
    took = time.perf_counter() - start
    stop.set()
    for w in watchers:
        w.join()
    held(vms, "read", arm, guest=False)
    vms.close()
    logs = run(arm, "logs", "reader", check=False)
    said = DD_SECONDS.search(logs.stdout + logs.stderr)
    if high == "short":
        print(f"{name}: budget {slice_budget() >> 10} KiB; {held_short(arm, run_id)}", file=sys.stderr, flush=True)
    run(arm, "rm", "-f", "reader", check=False)
    if waited.stdout.strip() != "0" or not said:
        print(f"{name}: failed: exit {waited.stdout.strip()}: {(logs.stdout + logs.stderr).strip()[-300:]}",
              file=sys.stderr, flush=True)
        return None
    print(f"{name}: dd {said[1]} s, its run {took:.2f} s", file=sys.stderr, flush=True)
    return float(said[1])


def slice_budget():
    """The slice's MemoryHigh as it stands, in bytes; 0 where it has none."""
    with open(os.path.join(CGROUP, "memory.high")) as f:
        high = f.read().strip()
    return int(high) if high.isdigit() else 0


def warm_in_slice(arm):
    """Whether a microVM of the arm's home's templates is in the slice: a VM process
    restored from one, its command line naming them."""
    templates = os.path.join(homes[arm], "templates").encode()
    for pid in vm_pids():
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                if templates in f.read():
                    return True
        except OSError:
            pass
    return False


def e2e_round(arm, i):
    """Round `i` of the E2E's phases for `arm` (--as-e2e), in a home of its own given the
    seed's images and guest kernel, under a slice made for it, which is stopped after: each
    read's name and seconds by its phase."""
    homes[arm] = os.path.join(work, f"home-{arm}-{i}")
    os.makedirs(homes[arm])
    for part in ("images", "guest"):
        if os.path.isdir(os.path.join(SEED, part)):
            shutil.copytree(os.path.join(SEED, part), os.path.join(homes[arm], part), symlinks=True)
    file = FILES[0]
    # Out of the host's page cache, as the E2E's file is new: the reads before kept it there.
    drop(os.path.join(vol, file))
    # The image's template saved under the slice, and its pool warm there.
    run(arm, "run", "--rm", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol", "alpine", "true")
    deadline = time.time() + 120
    while not warm_in_slice(arm):
        if time.time() > deadline:
            raise SystemExit(f"{arm}: no warm microVM in {SLICE}")
        time.sleep(0.02)
    def reads(high):
        names = [f"{high}-{arm}-{file}-{i}-{k + 1}" for k in range(a.reads)]
        return [(name, pooled(arm, file, high, name)) for name in names]

    took = {"infinity": reads("infinity")}
    past = slice_holds() + (128 << 20)
    set_budget(str(past))
    took["past128"] = reads("past128")
    set_budget("infinity")
    late = f"late-{arm}-{file}-{i}"
    # Its budget kept after, as the E2E keeps it while the short budget is worked out.
    try:
        late_s = one(arm, file, "late", late, budget=str(past), cached=True, keep=True)
    except SystemExit as e:
        print(f"{late}: failed: {str(e)[:300]}", file=sys.stderr, flush=True)
        run(arm, "rm", "-f", "reader", check=False)
        late_s = None
    took["late"] = [(late, late_s)]
    took["short"] = lone(arm, file, i)
    run(arm, "daemon", "stop", check=False, extra=POOL_NONE)
    try:
        shutil.copy(os.path.join(homes[arm], "daemon.log"), os.path.join(out, f"daemon-{arm}-{i}.log"))
    except OSError:
        pass
    systemctl("revert", SLICE)
    systemctl("stop", SLICE)
    shutil.rmtree(homes[arm], ignore_errors=True)
    return took


# The lone run's daemon keeps no warm microVM.
POOL_NONE = {"SHARDS_POOL": "0"}


def lone(arm, file, i):
    """The E2E's short phase (--as-e2e), as 23a66853's E2E has it: its daemon started again
    keeping no warm microVM, a run of its own alone in the slice, its guest's way to the
    share warmed by a small file's read; then a budget a mebibyte short of what the slice
    holds with it running (`short_budget`), the daemon's word that the run is held within
    the least room, and the run's reads of the file, each an exec into it: each read's name
    and seconds as its dd says them, or None. A short budget worked out with no run in the
    slice, as this harness had it, once came to 28 MiB, the daemon having ended the slice's
    warm microVMs for room: the next run's vCPUs were throttled at memory.high for 2.5 hours,
    its template's working set 167 MiB (DAX budget run 40085529809)."""
    run(arm, "daemon", "stop", check=False)
    set_budget("infinity")
    with open(os.path.join(vol, "small"), "wb") as f:
        f.write(b"\1" * (1 << 20))
    run_id = run(arm, "run", "-d", "--name", "short", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol", "alpine",
                 "sleep", "100000", extra=POOL_NONE).stdout.strip()
    run(arm, "exec", "short", "dd", "if=/vol/small", "of=/dev/null", "bs=1M", extra=POOL_NONE)
    set_budget(str(short_budget()))
    # The E2E waits for the daemon's word before it reads, and fails without it: here the
    # reads are measured either way, and each says whether it was held.
    deadline = time.time() + 120
    while not said_least(arm, run_id):
        if time.time() > deadline:
            print(f"short-{arm}-{file}-{i}: not said held within the least room in 120 s", file=sys.stderr,
                  flush=True)
            break
        time.sleep(0.02)
    took = []
    for k in range(a.reads):
        name = f"short-{arm}-{file}-{i}-{k + 1}"
        vms = open(os.path.join(out, f"{name}.vms"), "w")
        held(vms, "started", arm, guest=False)
        stop = threading.Event()
        watchers = [threading.Thread(target=sample, args=(os.path.join(out, f"{name}.txt"), stop)),
                    threading.Thread(target=guest_sample,
                                     args=(arm, os.path.join(out, f"{name}.guest"), stop, GUEST_LIGHT))]
        for w in watchers:
            w.start()
        try:
            done = run(arm, "exec", "short", "dd", f"if=/vol/{file}", "of=/dev/null", "bs=1M", check=False,
                       timeout=1800, extra=POOL_NONE)
            said = DD_SECONDS.search(done.stdout + done.stderr) if done.returncode == 0 else None
        except subprocess.TimeoutExpired:
            said = None
        stop.set()
        for w in watchers:
            w.join()
        held(vms, "read", arm, guest=False)
        vms.close()
        print(f"{name}: budget {slice_budget() >> 10} KiB; {held_short(arm, run_id)}", file=sys.stderr, flush=True)
        took.append((name, float(said[1]) if said else None))
    run(arm, "rm", "-f", "short", check=False, extra=POOL_NONE)
    return took


def said_least(arm, run_id):
    """Whether the arm's daemon has said run `run_id` is held within the least room."""
    try:
        with open(os.path.join(homes[arm], "daemon.log"), errors="replace") as f:
            return any(f"container {run_id}:" in l and "the least room a run goes on in" in l for l in f)
    except OSError:
        return False


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
if a.as_e2e:
    # The images and guest kernel pulled once, by the first arm's build, for every round's
    # homes; its daemon stopped, so that none holds the store as it is copied.
    first = next(iter(ARMS))
    homes[first] = SEED
    run(first, "pull", "-q", "alpine")
    run(first, "daemon", "stop", check=False)
    run(first, "run", "--rm", "--pull", "never", "alpine", "true")
    run(first, "daemon", "stop", check=False)
    shutil.rmtree(os.path.join(SEED, "templates"), ignore_errors=True)
    times = {(high, arm, FILES[0]): [] for high in ("infinity", "past128", "late", "short") for arm in ARMS}
    failed = {case: 0 for case in times}
    arms = list(ARMS)
    for i in range(a.rounds):
        # Each arm first in turn, so that each meets the host as the others do.
        for arm in arms[i % len(arms):] + arms[: i % len(arms)]:
            for high, xs in e2e_round(arm, i + 1).items():
                for name, took in xs:
                    if took is None:
                        failed[(high, arm, FILES[0])] += 1
                        continue
                    times[(high, arm, FILES[0])].append(took)
                    print(f"round {i + 1} {high} {arm} {FILES[0]}: {took:.2f} s {name}", file=sys.stderr, flush=True)
else:
    cases = [(high, arm, file) for high in a.highs.split(",") for file in FILES for arm in ARMS]
    times = {case: [] for case in cases}
    for i in range(a.rounds):
        for high, arm, file in cases[i % len(cases):] + cases[: i % len(cases)]:
            took = one(arm, file, high, f"{high}-{arm}-{file}-{i + 1}")
            times[(high, arm, file)].append(took)
            print(f"round {i + 1} {high} {arm} {file}: {took:.2f} s", file=sys.stderr, flush=True)
for (high, arm, file), xs in times.items():
    lost = f"; failed {failed[(high, arm, file)]}" if a.as_e2e and failed[(high, arm, file)] else ""
    if not xs:
        print(f"high {high:<8} {arm:<6} {file:<7} n 0{lost}")
        continue
    print(f"high {high:<8} {arm:<6} {file:<7} n {len(xs)} p50 {pct(xs, .5):8.2f} p90 {pct(xs, .9):8.2f} "
          f"p99 {pct(xs, .99):8.2f} max {max(xs):8.2f} s{lost}")
