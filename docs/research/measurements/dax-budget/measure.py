#!/usr/bin/env python3
"""What a microVM held to a budget pays to read a file through its volume's DAX window
(D188, D189): under a slice of the user's systemd manager whose MemoryHigh is past the
microVM idle and short of the file, `dd` of a 1 GiB file from a volume, its share's window
as long as the host's memory (the default) and with none (SHARDS_SHARE_WINDOW_MIB=0: FUSE,
through the guest's page cache), the file sparse and written out. Rounds interleave the
four, each first in turn; the file's pages are out of the host's page cache before each
read, so that the read's microVM brings them in and pays for them.

    measure.py SHARDS OUT [--rounds N] [--high 384M] [--size-mib 1024] [--perf]

Prints n, p50, p90, p99 and max of each read's seconds, with host, OS and revision. Into
OUT, for each read, what its slice held each second (memory.current, memory.stat's file,
anon and reclaim counters, memory.events, memory.pressure), its VM process's vCPU threads'
states and CPU times, and KVM's counters of its VM where `sudo -n` reads debugfs; with
--perf, a profile of the whole host 5 to 15 s into each read (`sudo -n perf`).

Linux, with KVM and a user's systemd manager (XDG_RUNTIME_DIR's bus). Each arm has a home
of its own, which pulls alpine once, and an empty DOCKER_CONFIG; all of it is removed, and
its daemons and the slice stopped, however the run ends.
"""
import argparse, atexit, os, platform, shutil, subprocess, sys, tempfile, threading, time

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("out")
p.add_argument("--rounds", type=int, default=3)
p.add_argument("--high", default="384M")
p.add_argument("--size-mib", type=int, default=1024)
p.add_argument("--perf", action="store_true")
a = p.parse_args()

shards = os.path.realpath(a.shards)
out = os.path.realpath(a.out)
os.makedirs(out, exist_ok=True)
work = tempfile.mkdtemp(prefix="dax-budget-")
config, vol = os.path.join(work, "docker-config"), os.path.join(work, "vol")
os.makedirs(config)
os.makedirs(vol)
SLICE = f"shardsdax{os.getpid()}.slice"
uid = os.getuid()
CGROUP = f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{SLICE}"
ARMS = {"window": {}, "none": {"SHARDS_SHARE_WINDOW_MIB": "0"}}
FILES = ("sparse", "written")
homes = {arm: os.path.join(work, f"home-{arm}") for arm in ARMS}


def env(arm):
    e = dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config)
    e.pop("SHARDS_SHARE_WINDOW_MIB", None)
    e.update(ARMS[arm])
    return e


def run(arm, *args, check=True, timeout=None):
    done = subprocess.run([shards, *args], env=env(arm), capture_output=True, text=True, timeout=timeout)
    if check and done.returncode != 0:
        raise SystemExit(f"{arm}: shards {' '.join(args)}: exit {done.returncode}\n{done.stdout}{done.stderr}")
    return done


def systemctl(*args):
    return subprocess.run(["systemctl", "--user", *args], capture_output=True, text=True)


@atexit.register
def clean():
    """However the run ends, a failed step's exit included: each arm's daemon stopped, its
    runs with it, the slice stopped, and this run's own files removed."""
    for arm in ARMS:
        subprocess.run([shards, "daemon", "stop"], env=env(arm), capture_output=True)
    systemctl("stop", SLICE)
    shutil.rmtree(work, ignore_errors=True)


def drop(path):
    """The file's pages out of the host's page cache (clean, unmapped once no microVM
    maps it)."""
    fd = os.open(path, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


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
    with open(os.path.join(out, f"{name}.perf.txt"), "w") as f:
        subprocess.run(["sudo", "-n", "perf", "report", "-i", data, "--stdio", "--no-children",
                        "--percent-limit", "0.5"], stdout=f, stderr=subprocess.DEVNULL)


def one(arm, file, name):
    """One read's seconds: a microVM started in the slice, the budget set, the file read."""
    if not os.path.isdir(homes[arm]):
        run(arm, "pull", "-q", "alpine")
    run(arm, "run", "-d", "--name", "r", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol", "alpine",
        "sleep", "100000")
    set_ = systemctl("set-property", "--runtime", SLICE, f"MemoryHigh={a.high}", "MemorySwapMax=0")
    if set_.returncode != 0:
        raise SystemExit(f"the slice's budget: {set_.stderr}")
    drop(os.path.join(vol, file))
    stop = threading.Event()
    watchers = [threading.Thread(target=sample, args=(os.path.join(out, f"{name}.txt"), stop))]
    if a.perf:
        watchers.append(threading.Thread(target=profile, args=(name, stop)))
    for w in watchers:
        w.start()
    start = time.perf_counter()
    done = run(arm, "exec", "r", "dd", f"if=/vol/{file}", "of=/dev/null", "bs=1M", check=False, timeout=1800)
    took = time.perf_counter() - start
    stop.set()
    for w in watchers:
        w.join()
    run(arm, "rm", "-f", "r", check=False)
    systemctl("revert", SLICE)
    systemctl("stop", SLICE)
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
      f"revision {rev}; budget {a.high}, file {a.size_mib} MiB")
cases = [(arm, file) for file in FILES for arm in ARMS]
times = {case: [] for case in cases}
for i in range(a.rounds):
    for arm, file in cases[i % len(cases):] + cases[: i % len(cases)]:
        took = one(arm, file, f"{arm}-{file}-{i + 1}")
        times[(arm, file)].append(took)
        print(f"round {i + 1} {arm} {file}: {took:.2f} s", file=sys.stderr, flush=True)
for (arm, file), xs in times.items():
    print(f"{arm:<6} {file:<7} n {len(xs)} p50 {pct(xs, .5):8.2f} p90 {pct(xs, .9):8.2f} "
          f"p99 {pct(xs, .99):8.2f} max {max(xs):8.2f} s")
