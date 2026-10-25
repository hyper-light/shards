#!/usr/bin/env python3
"""Whether a volume's DAX window maps its ranges better on 2 MiB folios (D189's Next, PM
M308: an Intel runner read 1 GiB five times faster sparse through them, a third faster
written), measured on this host for what they cost too: each range's first touch a 2 or
4 MiB read where a page's, a 2 MiB folio held, a store dirtying the folio it lands in, a run
held to a budget. Arms, each a build of shards and a home of its own:
  window       the tree's: each range as its host's kernel faults it
  huge         dax-budget's huge.patch: each range advised onto huge pages (MADV_HUGEPAGE),
               a fault reading two 2 MiB folios ahead (mm/filemap.c do_sync_mmap_readahead)
  huge-random  huge-random.patch: MADV_HUGEPAGE and MADV_RANDOM, one folio a fault
  fuse         the tree's with no window (SHARDS_SHARE_WINDOW_MIB=0), for reference only
Every window arm's window is as long as the host's memory, given (none is the default on
a virtual machine on an AMD CPU, D189); every daemon keeps no pool (SHARDS_POOL=0), so that
each run restores the arm's template alone; each home pulls IMAGE once and has an empty
DOCKER_CONFIG.

    measure.py OUT --window SHARDS [--huge SHARDS] [--huge-random SHARDS]
               [--arms window,huge,huge-random] [--cells seq,touch,write,budget]
               [--rounds 6] [--n 1000] [--touch-gib 4] [--seq-mib 1024] [--high 384M]
               [--image IMAGE] [--work DIR] [--parent-slice NAME]

The files, in WORK/vol on the file system measured (WORK: --work, kept, or a directory of
its own beside OUT, removed): `sparse` (--seq-mib, a hole), `written` (--seq-mib of random
bytes), `touch` (--touch-gib GiB of random bytes), each synced; `guestrand`
(guestrand.c, static, built by zig for this host's architecture). Each arm's first run, in
the slice, saves the image's template with the volume, and a second restores it reading
`written` whole, as the readers' runs do. Each round then runs every cell for every arm,
the arms' order turning each round, each cell a run of its own (`run -d` in a slice of the
user's manager, `--cgroup-parent`), its files synced and out of the host's page cache
first (POSIX_FADV_DONTNEED), its workload `exec`'d in it (with --parent-slice, the slice is
NAME.slice's child, which a shared host holds to a MemoryMax of its own):
  seq-sparse, seq-written  guestrand seq: the file read whole, a MiB a read(2): seconds
  touch    guestrand touch /vol/touch N SEED: a 4 KiB pread at a random page of each of N
           distinct ranges (first), then at another page of each (second): µs a read
  write    guestrand write /vol/touch N SEED: a 4 KiB pwrite at a random page of each of N
           distinct ranges, then fsync: µs a write; the bytes the volume's block device
           wrote (/proc/diskstats) from before the run to after a syncfs(2) past its end,
           per write
  budget-seq, budget-touch  seq-written and touch with the slice's MemoryHigh at --high
           (as M308), no swap, set once the run has started
SEED is the round's, the same for every arm. Around each workload: the VM process's
mappings of the cell's file (/proc/PID/smaps: their count, Rss, FilePmdMapped, and how many
are VmFlags hg and rr), KVM's counters of its VM where `sudo -n` reads debugfs (pf_taken
across the workload, pages_4k and pages_2m after it), the slice's peak and last
memory.stat file, file_thp and file_dirty plus file_writeback (sampled every 0.1 s), its
memory.events across the run, and the bytes of the file the host's page cache holds after
it (mincore(2)). A cell that fails is said (FAILED) and counted; the rest go on.

Prints a header (host, OS, kernel, CPU model and vendor, whether a hypervisor is beneath,
revision, the host's memory, THP's settings, the volume's file system, device and
read_ahead_kb, and on Btrfs the kernel's CONFIG_BTRFS_EXPERIMENTAL), then a `cell` line
for each run, its fields KEY=VALUE, for rule.py.

The decision rule, fixed before any number was taken. For each host and each arm A of huge
and huge-random against window, each comparison over the arms' per-round values, A worse
beyond the rounds' spread where its whole range lies above window's (its least above
window's most), better beyond it where its whole range lies below:
  1. seq: neither file's seconds worse beyond the spread, and at least one file's better;
  2. touch (first and second) and write: µs a read or a write, p50 and p99, no worse
     beyond the spread;
  3. write: bytes written back per write no worse beyond the spread;
  4. budget: every run finished and none killed (memory.events oom_kill 0), budget-seq's
     seconds and budget-touch's first p50 and p99 no worse beyond the spread.
Memory held (the slice's file and file_thp, the file's bytes in the page cache) is
recorded and judged through 4. A passes on a host where all four hold. A class of hosts
(Intel virtual machines; AMD virtual machines, a window given; bare metal; each with its
volume's file system) takes the arm that passes on every host of it measured (where both
do, the one whose seq-written median is less), none where neither does, the comparisons
that failed its reason; a class not measured keeps none.

Linux, with KVM and a user's systemd manager (XDG_RUNTIME_DIR's bus); zig on PATH.
"""
import argparse, atexit, ctypes, glob, mmap, os, platform, shutil, signal, subprocess, sys, tempfile, threading, time

here = os.path.dirname(os.path.realpath(__file__))
p = argparse.ArgumentParser()
p.add_argument("out")
p.add_argument("--window", required=True)
p.add_argument("--huge")
p.add_argument("--huge-random")
p.add_argument("--arms", default="window,huge,huge-random")
p.add_argument("--cells", default="seq,touch,write,budget")
p.add_argument("--rounds", type=int, default=6)
p.add_argument("--n", type=int, default=1000)
p.add_argument("--touch-gib", type=int, default=4)
p.add_argument("--seq-mib", type=int, default=1024)
p.add_argument("--high", default="384M")
p.add_argument("--image", default="mirror.gcr.io/library/alpine")
p.add_argument("--work")
p.add_argument("--parent-slice")
a = p.parse_args()

out = os.path.realpath(a.out)
os.makedirs(out, exist_ok=True)
keep = bool(a.work)
work = os.path.realpath(a.work) if a.work else tempfile.mkdtemp(prefix=".dax-huge-", dir=os.path.dirname(out))
config, vol = os.path.join(work, "docker-config"), os.path.join(work, "vol")
os.makedirs(config, exist_ok=True)
os.makedirs(vol, exist_ok=True)
# Under --parent-slice, the slice is its child (systemd.slice(5): a dash nests a slice in the
# one its name begins with).
PARENT = f"{a.parent_slice}.slice/" if a.parent_slice else ""
SLICE = f"{a.parent_slice}-shardshuge{os.getpid()}.slice" if a.parent_slice else f"shardshuge{os.getpid()}.slice"
uid = os.getuid()
CGROUP = f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{PARENT}{SLICE}"
HOST_MIB = (os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") >> 20) & ~1
WINDOW = {"SHARDS_SHARE_WINDOW_MIB": str(HOST_MIB)}
BUILDS = {"window": (a.window, WINDOW), "huge": (a.huge, WINDOW), "huge-random": (a.huge_random, WINDOW),
          "fuse": (a.window, {"SHARDS_SHARE_WINDOW_MIB": "0"})}
ARMS = {}
for arm in a.arms.split(","):
    if arm not in BUILDS or not BUILDS[arm][0]:
        raise SystemExit(f"arm {arm}: unknown, or its build not given")
    ARMS[arm] = (os.path.realpath(BUILDS[arm][0]), BUILDS[arm][1])
KINDS = []
for cell in a.cells.split(","):
    KINDS += {"seq": ["seq-sparse", "seq-written"], "touch": ["touch"], "write": ["write"],
              "budget": ["budget-seq", "budget-touch"]}[cell]
homes = {arm: os.path.join(work, f"home-{arm}") for arm in ARMS}
OUT = open(os.path.join(out, "out.txt"), "a")


def say(line):
    print(line, flush=True)
    OUT.write(line + "\n")
    OUT.flush()


def env(arm):
    e = dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config, SHARDS_POOL="0")
    e.pop("SHARDS_SHARE_WINDOW_MIB", None)
    e.update(ARMS[arm][1])
    return e


def run(arm, *args, check=True, timeout=None):
    done = subprocess.run([ARMS[arm][0], *args], env=env(arm), capture_output=True, text=True, timeout=timeout)
    if check and done.returncode != 0:
        raise RuntimeError(f"{arm}: shards {' '.join(args)}: exit {done.returncode}\n{done.stdout}{done.stderr}")
    return done


def systemctl(*args):
    return subprocess.run(["systemctl", "--user", *args], capture_output=True, text=True)


@atexit.register
def clean():
    """However the run ends: each arm's daemon stopped, its runs with it, its log kept in
    OUT; the slice stopped; WORK removed unless it was given."""
    for arm in ARMS:
        subprocess.run([ARMS[arm][0], "stop", "daemon"], env=env(arm), capture_output=True)
        try:
            shutil.copy(os.path.join(homes[arm], "daemon.log"), os.path.join(out, f"daemon-{arm}.log"))
        except OSError:
            pass
    systemctl("revert", SLICE)
    systemctl("stop", SLICE)
    if not keep:
        shutil.rmtree(work, ignore_errors=True)


# Ended from outside (a CI job cancelled at its time limit): ended as from within, so that
# `clean` keeps the daemons' logs.
signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))

LIBC = ctypes.CDLL(None, use_errno=True)
LIBC.mmap.restype = ctypes.c_void_p
LIBC.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
LIBC.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
LIBC.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p]
LIBC.syncfs.argtypes = [ctypes.c_int]
LOW_BIT = bytes(i & 1 for i in range(256))


def cached(path):
    """Of file `path`, the bytes its host's page cache holds: mincore(2) of a shared mapping
    of it, which faults none in and, to its owner, says the page cache's (mm/mincore.c
    `can_do_mincore`)."""
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        if size == 0:
            return 0
        addr = LIBC.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        if addr is None or addr == ctypes.c_void_p(-1).value:
            raise OSError(ctypes.get_errno(), "mmap")
        try:
            page = os.sysconf("SC_PAGE_SIZE")
            vec = ctypes.create_string_buffer((size + page - 1) // page)
            if LIBC.mincore(ctypes.c_void_p(addr), size, vec) != 0:
                raise OSError(ctypes.get_errno(), "mincore")
            return vec.raw.translate(LOW_BIT).count(1) * page
        finally:
            LIBC.munmap(ctypes.c_void_p(addr), size)
    finally:
        os.close(fd)


def syncfs():
    fd = os.open(vol, os.O_RDONLY | os.O_DIRECTORY)
    try:
        if LIBC.syncfs(fd) != 0:
            raise OSError(ctypes.get_errno(), "syncfs")
    finally:
        os.close(fd)


def drop(path):
    """The file's pages out of the host's page cache: written back first, then let go of
    (clean and mapped by no microVM, as each cell's run has ended)."""
    syncfs()
    fd = os.open(path, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


def mount_of(path):
    """The mount `path` is on (/proc/self/mountinfo, the longest mount point it lies
    under): its file system type, its source and its device number."""
    path, best = os.path.realpath(path), None
    with open("/proc/self/mountinfo") as f:
        for line in f:
            fields = line.split()
            dash = fields.index("-")
            point = fields[4].replace("\\040", " ")
            if (path == point or path.startswith(point.rstrip("/") + "/")) and (best is None or len(point) > len(best[0])):
                best = (point, fields[dash + 1], fields[dash + 2], fields[2])
    return best[1:] if best else ("-", "-", "-")


def block_device(source, devno):
    """The block device the volume's file system writes to, as /proc/diskstats names it:
    its mount's source where that is a block device (Btrfs' own device numbers are
    anonymous), else its device number's (a source of /dev/root names none)."""
    for name in (os.path.basename(os.path.realpath(source)) if source.startswith("/dev/") else None,
                 os.path.basename(os.path.realpath(f"/sys/dev/block/{devno}"))):
        if name and os.path.exists(f"/sys/class/block/{name}"):
            return name
    return None


def written_bytes(dev):
    """The bytes block device `dev` has written (/proc/diskstats' sectors written, of 512
    bytes each), or None where it has no line."""
    if not dev:
        return None
    with open("/proc/diskstats") as f:
        for line in f:
            fields = line.split()
            if len(fields) > 9 and fields[2] == dev:
                return int(fields[9]) * 512
    return None


def read(path, default="-"):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return default


def btrfs_experimental():
    """CONFIG_BTRFS_EXPERIMENTAL of the running kernel's config (/boot/config-RELEASE or
    /proc/config.gz)."""
    text = read(f"/boot/config-{platform.release()}", "")
    if not text:
        try:
            import gzip
            with gzip.open("/proc/config.gz", "rt") as f:
                text = f.read()
        except OSError:
            return "unread"
    for line in text.splitlines():
        if line.startswith("CONFIG_BTRFS_EXPERIMENTAL="):
            return line.split("=", 1)[1]
        if line == "# CONFIG_BTRFS_EXPERIMENTAL is not set":
            return "n"
    return "absent"


def header():
    info = read("/proc/cpuinfo")
    first = lambda key: next((l.split(":", 1)[1].strip() for l in info.splitlines() if l.startswith(key)), "-")
    flags = first("flags").split()
    rev = subprocess.run(["git", "-C", here, "rev-parse", "--short", "HEAD"], capture_output=True,
                         text=True).stdout.strip() or "unknown"
    osname = platform.freedesktop_os_release().get("PRETTY_NAME", "Linux")
    fstype, source, devno = mount_of(vol)
    dev = block_device(source, devno)
    thp = {name: read(f"/sys/kernel/mm/transparent_hugepage/{name}") for name in
           ("enabled", "defrag", "shmem_enabled", "khugepaged/defrag")}
    say(f"host {platform.node()}; {osname}; kernel {platform.release()}; {platform.machine()}; "
        f"{first('model name')}; vendor {first('vendor_id')}; hypervisor {'yes' if 'hypervisor' in flags else 'no'}; "
        f"shards {rev}; memory {HOST_MIB} MiB; cpus {os.cpu_count()} (this run's {len(os.sched_getaffinity(0))})")
    say("thp " + "; ".join(f"{k} {v}" for k, v in thp.items()))
    # The read-ahead a fault reads around it (mm/filemap.c, its file's ra_pages): its file
    # system's backing device's, else its block device's queue's (a partition's, its disk's).
    st = os.stat(vol)
    ra = read(f"/sys/class/bdi/{os.major(st.st_dev)}:{os.minor(st.st_dev)}/read_ahead_kb")
    if ra == "-" and dev:
        ra = read(f"/sys/class/block/{dev}/queue/read_ahead_kb")
        if ra == "-":
            ra = read(f"/sys/class/block/{dev}/../queue/read_ahead_kb")
    say(f"volume {vol}: {fstype} on {source} (device {dev or '-'}, read_ahead_kb {ra})"
        + (f"; CONFIG_BTRFS_EXPERIMENTAL {btrfs_experimental()}" if fstype == "btrfs" else ""))
    say(f"arms {','.join(ARMS)}; cells {','.join(KINDS)}; rounds {a.rounds}; n {a.n}; touch {a.touch_gib} GiB; "
        f"seq {a.seq_mib} MiB; high {a.high}; window {HOST_MIB} MiB; image {a.image}")
    return dev


def lay_out():
    """The volume's files, each this size already or written again."""
    def ensure(name, size, fill):
        path = os.path.join(vol, name)
        if os.path.exists(path) and os.path.getsize(path) == size:
            return
        with open(path, "wb") as f:
            if fill:
                for _ in range(size >> 20):
                    f.write(os.urandom(1 << 20))
            else:
                f.truncate(size)
            f.flush()
            os.fsync(f.fileno())
    ensure("sparse", a.seq_mib << 20, False)
    ensure("written", a.seq_mib << 20, True)
    ensure("touch", a.touch_gib << 30, True)
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine().lower()]
    subprocess.run(["zig", "cc", "-target", f"{arch}-linux-musl", "-static", "-O2", "-Wall", "-Wextra", "-Werror",
                    os.path.join(here, "guestrand.c"), "-o", os.path.join(vol, "guestrand")], check=True)


def stat_of(pid):
    """Process `pid`'s state and start time (proc_pid_stat(5) fields 3 and 22), or None
    once it is gone."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
        return fields[0], fields[19]
    except (OSError, IndexError):
        return None


def ended(pid, started, timeout=120):
    """Waits until process `pid`, started at `started`, has ended: gone, a zombie, or its
    number another's."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        now = stat_of(pid)
        if now is None or now[0] in ("Z", "X") or now[1] != started:
            return
        time.sleep(0.02)
    raise RuntimeError(f"VM process {pid} still runs {timeout} s after its run was removed")


def template(arm):
    """The arm's home, its image pulled and its template saved with the volume, then
    restored once reading as the cells do (which records the working set a restore maps
    ahead: dax-budget's measure.py says why each)."""
    if glob.glob(os.path.join(homes[arm], "templates", "*", "current")):
        return
    run(arm, "pull", "-q", a.image)
    run(arm, "run", "-d", "--name", "warm", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol", "--pull", "never",
        a.image, "sleep", "600")
    pid = run(arm, "inspect", "-f", "{{.State.Pid}}", "warm").stdout.strip()
    started = stat_of(pid)
    deadline = time.time() + 300
    while not glob.glob(os.path.join(homes[arm], "templates", "*", "current")):
        if time.time() > deadline:
            raise RuntimeError(f"{arm}: no template saved in 300 s")
        time.sleep(0.5)
    run(arm, "rm", "-f", "warm", check=False)
    if started:
        ended(pid, started[1])
    run(arm, "run", "--rm", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol", "--pull", "never", a.image,
        "/vol/guestrand", "seq", "/vol/written")


def smaps(pid, path):
    """The VM process's mappings of file `path`: how many, their Rss and FilePmdMapped in
    KiB, and how many are VmFlags hg (MADV_HUGEPAGE) and rr (MADV_RANDOM)."""
    path, n, rss, pmd, hg, rr, mine = os.path.realpath(path), 0, 0, 0, 0, 0, False
    try:
        with open(f"/proc/{pid}/smaps") as f:
            for line in f:
                words = line.split()
                if not words:
                    continue
                if "-" in words[0] and not words[0].endswith(":"):
                    mine = len(words) > 5 and words[5] == path
                    n += mine
                elif mine and words[0] == "Rss:":
                    rss += int(words[1])
                elif mine and words[0] == "FilePmdMapped:":
                    pmd += int(words[1])
                elif mine and words[0] == "VmFlags:":
                    hg += "hg" in words[1:]
                    rr += "rr" in words[1:]
    except OSError as e:
        return {"smaps": f"unread:{e.errno}"}
    return {"maps": n, "rss_kib": rss, "pmd_kib": pmd, "hg": hg, "rr": rr}


def kvm(pid):
    """KVM's counters of the VM of process `pid`, where `sudo -n` reads debugfs."""
    script = (f"for d in /sys/kernel/debug/kvm/{pid}-*; do for f in pf_taken pages_4k pages_2m pages_1g; do "
              "[ -f $d/$f ] && printf '%s %s ' $f $(cat $d/$f); done; done")
    done = subprocess.run(["sudo", "-n", "sh", "-c", script], capture_output=True, text=True)
    words = done.stdout.split()
    return dict(zip(words[::2], (int(w) for w in words[1::2])))


def slice_stat():
    try:
        with open(os.path.join(CGROUP, "memory.stat")) as f:
            return dict((k, int(v)) for k, v in (line.split() for line in f))
    except (OSError, ValueError):
        return {}


def events():
    try:
        with open(os.path.join(CGROUP, "memory.events")) as f:
            return dict((k, int(v)) for k, v in (line.split() for line in f))
    except (OSError, ValueError):
        return {}


def sampler(peaks, stop):
    """The slice's file, file_thp and file_dirty plus file_writeback at their peaks, every
    0.1 s until `stop`."""
    while not stop.wait(0.1):
        s = slice_stat()
        for key, value in (("file", s.get("file")), ("file_thp", s.get("file_thp")),
                           ("dirty", None if "file_dirty" not in s else s["file_dirty"] + s.get("file_writeback", 0))):
            if value is not None:
                peaks[key] = max(peaks.get(key, 0), value)


def parse(lines):
    """guestrand's lines as fields: `first n N p50 P ...` as first_n, first_p50, ...;
    `seq bytes B s S` as bytes and s; `fsync s S` as fsync_s; `dax D`; meminfo's."""
    fields = {}
    for line in lines:
        w = line.split()
        if not w:
            continue
        if w[0] in ("first", "second", "write") and len(w) >= 11:
            for k, v in zip(w[1::2], w[2::2]):
                fields[f"{w[0]}_{k}"] = v
        elif w[0] == "seq" and len(w) >= 5:
            fields["bytes"], fields["s"] = w[2], w[4]
        elif w[0] == "fsync" and len(w) >= 3:
            fields["fsync_s"] = w[2]
        elif w[0] == "dax" and len(w) >= 2:
            fields["dax"] = w[1]
        elif w[0] == "meminfo" and len(w) >= 7:
            fields.update(guest_cached_kib=f"{w[2]}>{w[3]}", guest_free_kib=f"{w[5]}>{w[6]}")
    return fields


FAILED = []


def cell(r, arm, kind, seed, dev):
    """One cell: a run started in the slice, its budget set where the cell is budgeted, its
    file out of the host's page cache, its workload exec'd and timed, and what it held."""
    budget = kind.startswith("budget-")
    file = {"seq-sparse": "sparse", "seq-written": "written", "budget-seq": "written"}.get(kind, "touch")
    work_args = (["seq", f"/vol/{file}"] if kind.endswith(("seq", "sparse", "written"))
                 else [kind.removeprefix("budget-"), "/vol/touch", str(a.n), str(seed)])
    load = os.getloadavg()[0]
    pid = started = None
    stop, peaks = threading.Event(), {}
    watch = threading.Thread(target=sampler, args=(peaks, stop))
    try:
        run(arm, "run", "-d", "--name", "cell", "--cgroup-parent", SLICE, "-v", f"{vol}:/vol", "--pull", "never",
            a.image, "sleep", "100000")
        pid = run(arm, "inspect", "-f", "{{.State.Pid}}", "cell").stdout.strip()
        started = stat_of(pid)
        if budget:
            set_ = systemctl("set-property", "--runtime", SLICE, f"MemoryHigh={a.high}", "MemorySwapMax=0")
            if set_.returncode != 0:
                raise RuntimeError(f"the slice's budget: {set_.stderr}")
        drop(os.path.join(vol, file))
        before_events, before_kvm, before_written = events(), kvm(pid), written_bytes(dev)
        watch.start()
        t = time.perf_counter()
        done = run(arm, "exec", "cell", "/vol/guestrand", *work_args, check=False, timeout=1800)
        took = time.perf_counter() - t
        stop.set()
        watch.join()
        fields = parse(done.stdout.splitlines())
        said = {"seq": ("s",), "touch": ("first_p50", "second_p50"), "write": ("write_p50", "fsync_s")}[work_args[0]]
        if done.returncode != 0 or any(k not in fields for k in said):
            raise RuntimeError(f"guestrand exit {done.returncode}: {done.stdout.strip()} {done.stderr.strip()}")
        after_kvm, stat, after_events = kvm(pid), slice_stat(), events()
        fields.update(smaps(pid, os.path.join(vol, file)))
        fields["exec_s"] = f"{took:.3f}"
        if after_kvm:
            fields["kvm_pf_taken"] = after_kvm.get("pf_taken", 0) - before_kvm.get("pf_taken", 0)
            for k in ("pages_4k", "pages_2m", "pages_1g"):
                fields[f"kvm_{k}"] = after_kvm.get(k, "-")
        for k in ("file", "file_thp", "dirty"):
            fields[f"peak_{k}"] = peaks.get(k, "-")
        fields.update(file_end=stat.get("file", "-"), file_thp_end=stat.get("file_thp", "-"),
                      held=cached(os.path.join(vol, file)))
        for k in ("high", "max", "oom", "oom_kill"):
            fields[k] = after_events.get(k, 0) - before_events.get(k, 0)
        if kind == "write":
            syncfs()
            after_written = written_bytes(dev)
            if before_written is not None and after_written is not None:
                fields["written"] = after_written - before_written
                fields["written_per_write"] = (after_written - before_written) // a.n
        say(f"cell round={r} arm={arm} kind={kind} load={load:.2f} "
            + " ".join(f"{k}={v}" for k, v in fields.items()))
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as e:
        stop.set()
        if watch.is_alive():
            watch.join()
        FAILED.append((r, arm, kind))
        say(f"FAILED round={r} arm={arm} kind={kind}: " + " ".join(str(e).split())[:600])
    finally:
        run(arm, "rm", "-f", "cell", check=False)
        if pid and started:
            try:
                ended(pid, started[1])
            except RuntimeError as e:
                say(f"FAILED round={r} arm={arm} kind={kind}: {e}")
        if budget:
            systemctl("set-property", "--runtime", SLICE, "MemoryHigh=infinity")


dev = header()
lay_out()
for arm in ARMS:
    template(arm)
say(f"laid out and templates saved; load {os.getloadavg()[0]:.2f}")
arms = list(ARMS)
for r in range(1, a.rounds + 1):
    order = arms[(r - 1) % len(arms):] + arms[:(r - 1) % len(arms)]
    for kind in KINDS:
        for arm in order:
            cell(r, arm, kind, r, dev)
say(f"done; load {os.getloadavg()[0]:.2f}; cells failed {len(FAILED)}: {FAILED or '-'}")
