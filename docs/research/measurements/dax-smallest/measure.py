#!/usr/bin/env python3
"""From what size a guest's files are better mapped through a volume's DAX window than read
through FUSE alone (D189's DAX_SMALLEST, today one window range, 2 MiB, not measured below
it): files under 2 MiB through both, on this host.

One build of shards carrying smallest.patch, two homes holding IMAGE already (no pull per
run), each run's `shards` given an empty DOCKER_CONFIG:
  fuse    SHARDS_SHARE_WINDOW_MIB=0: no window, every file through FUSE
  window  the host's default window, or --window MIB, and SHARDS_DAX_SMALLEST=0: every
          regular file is mapped, whatever its size
For each size in --sizes, --files files of that size are laid out in DIR/sSIZE, written
whole with bytes from os.urandom and synced, owned by this user and writable by it (HVF
maps no file the server may not write, window::SHARED_NEEDS_WRITE). Each round runs every
(arm, size, cache) cell, in an order that turns each round: before each, the size's files
are left in the host's page cache, each read whole (`warm`), or not (`cold`: Linux's
POSIX_FADV_DONTNEED of each; on macOS msync(2) MS_INVALIDATE of a shared mapping of each);
then `run --rm -v DIR:/data IMAGE /data/guestsmall /data/sSIZE first again overwrite
rewrite stat` (guestsmall.c, built static by zig for the guest's architecture into DIR),
whose lines give each phase's n, p50, p90, p99 and max in µs, how many files statx calls
DAX, and the guest's Cached and MemFree before and after. The daemon's log, which the
patch's VM process writes to, gives each FUSE_SETUPMAPPING's parts (DAX-SETUP, DAX-MAP)
and removal (DAX-REMOVE), and at the run's end its windows' ranges now and at their peak,
its host mappings, its descriptors and its memory (DAX-END); on Linux the daemon's open
descriptors and inotify watches at their peak while each run ran, sampled every 100 ms
(DAX-DAEMON: the daemon watches each file a window maps); each run's are kept beside its
own lines in OUT. With --sets, the many-small-files sets too: SETS' files (the cargo
registry's size mix, --mix FILE of one size in bytes a line), and --small N files of 4 KiB
(more than a window has ranges), each under the default window and under --set-window MIB
(1,024: 512 ranges), through both arms, read once each (`first`), --set-rounds times, the
arms' order turning each round: the window arm once for each threshold of --set-smallest
(bytes, a comma list; SHARDS_DAX_SMALLEST, the files of that size and more mapped; 0,
every file), the rule's sets at S being its window arm at S. --mix-from DIR writes --mix
from the sizes of DIR's regular files of 2 MiB or less (a cargo registry's sources). An
empty --sizes runs the sets alone.
A run that fails is said, with what it and the daemon said, and counted; the rest go on.

Each run's header: host, OS, kernel, CPU model, revision, the host's load,
vm.max_map_count and fs.inotify.max_user_watches where Linux has them, and the window's
length.

The decision rule, fixed before any number was taken: DAX_SMALLEST is the smallest
measured size S such that at S and at every larger measured size, on the Mac and on
Fedora, the window arm's per-file p50 and p99 are no worse than the FUSE arm's beyond the
rounds' spread (the range of the per-round values of each) for first reads, cold and warm,
re-reads (`again`) and overwrites; the VM process's memory per mapped file is no more than
the FUSE arm's for that file; and the many-small-files sets at S fail no request, stay
within the host's mapping and watch limits, and take no longer than through FUSE. Where no
size under 2 MiB meets it, 2 MiB stays, now measured.

    measure.py SHARDS --fuse HOME --window HOME --dir DIR [--sizes 4k,16k,...]
               [--files N] [--rounds N] [--window-mib MIB] [--image IMAGE] [--out OUT]
               [--sets [--mix FILE | --mix-from DIR] [--small N] [--set-window MIB]
                       [--set-smallest BYTES,...] [--set-rounds N]]

Each home holds IMAGE already (`SHARDS_HOME=HOME shards pull IMAGE` once): runs never pull.
Each home's daemon is stopped first and last, so that each starts with its arm's settings,
and the window arm's again as a set's window changes; a daemon's log is read from where it
stood before each run.
"""
import argparse, os, platform, shutil, statistics, subprocess, sys, tempfile, threading, time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("--fuse", required=True)
p.add_argument("--window", required=True)
p.add_argument("--dir", required=True)
p.add_argument("--sizes", default="4k,16k,32k,64k,256k,1m,2044k,2m")
p.add_argument("--files", type=int, default=1000)
p.add_argument("--rounds", type=int, default=5)
p.add_argument("--window-mib", type=int, default=0, help="the window arm's window; 0, the host's default")
p.add_argument("--image", default="alpine")
p.add_argument("--out", default="")
p.add_argument("--caches", default="cold,warm")
p.add_argument("--sets", action="store_true")
p.add_argument("--mix", default="")
p.add_argument("--mix-from", default="")
p.add_argument("--small", type=int, default=0)
p.add_argument("--set-window", type=int, default=1024)
p.add_argument("--set-smallest", default="0")
p.add_argument("--set-rounds", type=int, default=1)
a = p.parse_args()

here = Path(__file__).resolve().parent
data = Path(a.dir).resolve()
data.mkdir(parents=True, exist_ok=True)
config = Path(tempfile.mkdtemp(prefix="dax-smallest-"))
macos = sys.platform == "darwin"
F_NOCACHE = 48  # sys/fcntl.h
out = open(a.out, "a") if a.out else None


def say(line):
    print(line, flush=True)
    if out:
        out.write(line + "\n")
        out.flush()


def size_of(word):
    word = word.strip().lower()
    for suffix, scale in (("k", 1 << 10), ("m", 1 << 20)):
        if word.endswith(suffix):
            return int(word[:-1]) * scale
    return int(word)


def label(size):
    if size % (1 << 20) == 0:
        return f"{size >> 20}m"
    return f"{size >> 10}k" if size % 1024 == 0 else str(size)


SIZES = [size_of(s) for s in a.sizes.split(",") if s]
CACHES = a.caches.split(",")
ARMS = ["fuse", "window"]


def env(arm, window_mib=None, smallest=0):
    home = a.fuse if arm == "fuse" else a.window
    e = dict(os.environ, SHARDS_HOME=str(Path(home).resolve()), DOCKER_CONFIG=str(config))
    for k in ("SHARDS_SHARE_WINDOW_MIB", "SHARDS_DAX_SMALLEST"):
        e.pop(k, None)
    if arm == "fuse":
        e["SHARDS_SHARE_WINDOW_MIB"] = "0"
    else:
        e["SHARDS_DAX_SMALLEST"] = str(smallest)
        mib = window_mib if window_mib is not None else a.window_mib
        if mib:
            e["SHARDS_SHARE_WINDOW_MIB"] = str(mib)
    return e


def stop(arm):
    subprocess.run([a.shards, "stop", "daemon"], env=env(arm), capture_output=True)


def build():
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}[platform.machine().lower()]
    to = data / "guestsmall"
    subprocess.run(["zig", "cc", "-target", f"{arch}-linux-musl", "-static", "-O2", "-Wall", "-Wextra",
                    "-Werror", str(here / "guestsmall.c"), "-o", str(to)], check=True)


def lay_out(dirname, sizes):
    """Files of `sizes` in DIR/dirname, each written whole with random bytes and synced,
    this user's and writable by it."""
    d = data / dirname
    if d.exists():
        shutil.rmtree(d)
    d.mkdir()
    for i, size in enumerate(sizes):
        path = d / f"f{i:06}"
        with open(path, "wb") as f:
            if macos:
                import fcntl
                fcntl.fcntl(f.fileno(), F_NOCACHE, 1)
            f.write(os.urandom(size))
            f.flush()
            os.fsync(f.fileno())
        os.chmod(path, 0o644)


def invalidate(path):
    """Drops a file's pages from macOS's unified buffer cache: msync(2) MS_INVALIDATE of a
    shared mapping of it, which an unprivileged process may ask (PM M302)."""
    import ctypes
    import ctypes.util
    libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
                          ctypes.c_int, ctypes.c_longlong]
    libc.msync.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    prot_read, map_shared, ms_invalidate = 0x1, 0x1, 0x2  # sys/mman.h
    size = path.stat().st_size
    if size == 0:
        return
    fd = os.open(path, os.O_RDONLY)
    try:
        at = libc.mmap(None, size, prot_read, map_shared, fd, 0)
        if at in (None, ctypes.c_void_p(-1).value):
            raise SystemExit(f"mmap of {path}: errno {ctypes.get_errno()}")
        try:
            if libc.msync(at, size, ms_invalidate) != 0:
                raise SystemExit(f"msync of {path}: errno {ctypes.get_errno()}")
        finally:
            libc.munmap(at, size)
    finally:
        os.close(fd)


def leave(dirname, cache):
    for path in sorted((data / dirname).iterdir()):
        if cache == "warm":
            with open(path, "rb") as f:
                while f.read(1 << 22):
                    pass
        elif macos:
            invalidate(path)
        else:
            fd = os.open(path, os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)


def sysctl(name):
    try:
        if macos:
            return subprocess.run(["sysctl", "-n", name], capture_output=True, text=True).stdout.strip() or "-"
        return Path("/proc/sys/" + name.replace(".", "/")).read_text().strip()
    except OSError:
        return "-"


def host_header():
    rev = subprocess.run(["git", "-C", str(here), "rev-parse", "--short", "HEAD"],
                         capture_output=True, text=True).stdout.strip() or "unknown"
    if macos:
        cpu = sysctl("machdep.cpu.brand_string")
        osname = "macOS " + platform.mac_ver()[0]
    else:
        cpu = next((l.split(":", 1)[1].strip() for l in Path("/proc/cpuinfo").read_text().splitlines()
                    if l.startswith("model name")), "-")
        osname = platform.freedesktop_os_release().get("PRETTY_NAME", "Linux") if hasattr(platform, "freedesktop_os_release") else "Linux"
    return (f"host {platform.node()}; {osname}; kernel {platform.release()}; {platform.machine()}; {cpu}; "
            f"shards {rev}; vm.max_map_count {sysctl('vm.max_map_count')}; "
            f"fs.inotify.max_user_watches {sysctl('fs.inotify.max_user_watches')}")


def log_of(arm):
    home = a.fuse if arm == "fuse" else a.window
    return Path(home).resolve() / "daemon.log"


FAILED = []


def daemon_counts(home):
    """The home's daemon's open descriptors and inotify watches, Linux's (/proc/PID/fd, and
    the `inotify wd:` lines of its inotify descriptors' fdinfo); None where it has none
    running or there is no /proc."""
    try:
        pid = int((Path(home).resolve() / "daemon.pid").read_text().split()[0])
        names = os.listdir(f"/proc/{pid}/fd")
    except (OSError, ValueError, IndexError):
        return None
    watches = 0
    for n in names:
        try:
            if os.readlink(f"/proc/{pid}/fd/{n}") == "anon_inode:inotify":
                info = Path(f"/proc/{pid}/fdinfo/{n}").read_text()
                watches += sum(1 for l in info.splitlines() if l.startswith("inotify wd:"))
        except OSError:
            continue
    return len(names), watches


def run(arm, dirname, phases, window_mib=None, smallest=0):
    """One run's lines: guestsmall's, and the daemon log's DAX lines it wrote meanwhile,
    and on Linux a DAX-DAEMON line of the daemon's descriptors and inotify watches at their
    peak while it ran (sampled every 100 ms); none of guestsmall's where it failed, which is
    said and counted."""
    log = log_of(arm)
    before = log.stat().st_size if log.exists() else 0
    home = a.fuse if arm == "fuse" else a.window
    peaks = [0, 0]
    stopped = threading.Event()

    def sample():
        while not stopped.is_set():
            got = daemon_counts(home)
            if got:
                peaks[0], peaks[1] = max(peaks[0], got[0]), max(peaks[1], got[1])
            stopped.wait(0.1)

    sampler = threading.Thread(target=sample, daemon=True)
    if not macos:
        sampler.start()
    t = time.monotonic()
    done = subprocess.run([a.shards, "run", "--rm", "--pull", "never", "-v", f"{data}:/data", a.image,
                           "/data/guestsmall", f"/data/{dirname}"] + phases,
                          env=env(arm, window_mib, smallest), capture_output=True, text=True)
    took = time.monotonic() - t
    stopped.set()
    if not macos:
        sampler.join()
    said = []
    if log.exists():
        with open(log, "rb") as f:
            f.seek(before)
            said = [l.decode(errors="replace").rstrip() for l in f]
    dax = [l.strip() for l in said if "DAX-" in l]
    if not macos:
        dax.append(f"DAX-DAEMON fds_peak {peaks[0]} inotify_watches_peak {peaks[1]}")
    if done.returncode != 0:
        FAILED.append((arm, dirname))
        say(f"FAILED {arm} {dirname}: status {done.returncode}")
        for l in (done.stdout + done.stderr).splitlines() + ["the daemon said:"] + said:
            say(f"  | {l}")
        return [], dax, took
    return done.stdout.splitlines(), dax, took


def parse(lines):
    phases, dax, meminfo = {}, None, None
    for l in lines:
        w = l.split()
        if w and w[0] in ("first", "again", "overwrite", "rewrite", "stat") and len(w) == 6:
            phases[w[0]] = [int(w[1])] + [float(x) for x in w[2:]]
        elif w[:1] == ["dax"]:
            dax = (int(w[1]), int(w[3]))
        elif w[:1] == ["meminfo"]:
            meminfo = l
    return phases, dax, meminfo


def dax_summary(lines):
    """Of the DAX lines: mappings, removals, their parts' p50s, and the END line's figures."""
    maps = [l for l in lines if "DAX-MAP " in l]
    setups = [l for l in lines if "DAX-SETUP " in l]
    removes = [l for l in lines if "DAX-REMOVE " in l]
    ends = [l for l in lines if "DAX-END " in l]
    daemon = [l.split("DAX-DAEMON ", 1)[1] for l in lines if "DAX-DAEMON " in l]
    refused = sum(1 for l in setups if not l.endswith("errno 0"))

    def field(l, name):
        w = l.split()
        return float(w[w.index(name) + 1]) if name in w else None

    parts = {}
    for name in ("descriptor_us", "total_us"):
        v = [field(l, name) for l in setups]
        v = [x for x in v if x is not None]
        if v:
            parts[name] = statistics.median(v)
    for name in ("mmap_us", "stage2_us", "held_us"):
        v = [field(l, name) for l in maps]
        v = [x for x in v if x is not None]
        if v:
            parts[name] = statistics.median(v)
    end = " | ".join(l.split("DAX-END ", 1)[1] for l in ends)
    p50s = " ".join(f"{k} {v:.0f}" for k, v in parts.items())
    said = f"; daemon {daemon[0]}" if daemon else ""
    return f"setups {len(setups)} refused {refused} removes {len(removes)} p50s: {p50s or '-'}; end: {end or '-'}{said}"


if not shutil.which("zig"):
    raise SystemExit("zig is needed to build guestsmall (scripts/install-zig)")
if a.mix_from:
    sizes = []
    for root, _, files in os.walk(a.mix_from):
        for f in files:
            st = os.lstat(os.path.join(root, f))
            if (st.st_mode & 0o170000) == 0o100000 and st.st_size <= (2 << 20):
                sizes.append(st.st_size)
    a.mix = a.mix or str(data / "mix.txt")
    Path(a.mix).write_text("\n".join(map(str, sizes)) + "\n")
for arm in ARMS:
    stop(arm)
build()
say(host_header())
say(f"sizes {','.join(label(s) for s in SIZES)}; {a.files} files a size; {a.rounds} rounds; "
    f"caches {','.join(CACHES)}; window arm's window {a.window_mib or 'the default'} MiB")
for s in SIZES:
    lay_out(f"s{label(s)}", [s] * a.files)
cells = [(arm, s, c) for s in SIZES for arm in ARMS for c in CACHES]
got = {}
set_runs = {}
try:
    for r in range(a.rounds if cells else 0):
        k = r % len(cells)
        order = cells[k:] + cells[:k]
        if r % 2:
            order = order[::-1]
        for arm, s, cache in order:
            name = f"s{label(s)}"
            leave(name, cache)
            load = os.getloadavg()[0]
            lines, dax, took = run(arm, name, ["first", "again", "overwrite", "rewrite", "stat"])
            phases, statx, meminfo = parse(lines)
            if not lines:
                continue
            say(f"round {r} {arm} {label(s)} {cache} load {load:.2f} run {took:.2f}s statx-dax {statx[0] if statx else '?'} of {statx[1] if statx else '?'}")
            for ph, row in phases.items():
                got.setdefault((arm, s, cache, ph), []).append(row)
                say(f"  {ph:9} n {row[0]} p50 {row[1]:.1f} p90 {row[2]:.1f} p99 {row[3]:.1f} max {row[4]:.1f}")
            if meminfo:
                say(f"  {meminfo}")
            say(f"  {dax_summary(dax)}")
            if out:
                for l in dax:
                    out.write(f"    {l}\n")
    if a.sets:
        sets = []
        if a.mix:
            mix = [int(l) for l in Path(a.mix).read_text().split() if l.strip()]
            mix = [m for m in mix if m <= (2 << 20)]
            lay_out("mix", mix)
            sets.append(("mix", len(mix)))
        if a.small:
            lay_out("small", [4096] * a.small)
            sets.append(("small", a.small))
        thresholds = [int(t) for t in a.set_smallest.split(",") if t]
        variants = [("fuse", None)] + [("window", t) for t in thresholds]
        for r in range(a.set_rounds):
            k = r % len(variants)
            order = variants[k:] + variants[:k]
            for name, n in sets:
                for window_mib in (a.window_mib, a.set_window):
                    for arm, smallest in (order if r % 2 == 0 else order[::-1]):
                        # The window arm's daemon started again with this set's window and
                        # threshold.
                        if arm == "window":
                            stop(arm)
                        leave(name, "cold")
                        load = os.getloadavg()[0]
                        lines, dax, took = run(arm, name, ["first"], window_mib if arm == "window" else None,
                                               smallest or 0)
                        if not lines:
                            continue
                        phases, statx, _ = parse(lines)
                        row = phases.get("first", [0, 0, 0, 0, 0])
                        set_runs.setdefault((name, window_mib or 0, arm, smallest), []).append(took)
                        say(f"set {name} ({n} files) round {r} {arm} window {window_mib or 'default'} smallest "
                            f"{'-' if smallest is None else smallest} load {load:.2f} run {took:.2f}s "
                            f"statx-dax {statx[0] if statx else '?'} of {statx[1] if statx else '?'} "
                            f"first p50 {row[1]:.1f} p90 {row[2]:.1f} p99 {row[3]:.1f} max {row[4]:.1f}; {dax_summary(dax)}")
finally:
    for arm in ARMS:
        stop(arm)
    shutil.rmtree(config, ignore_errors=True)
say(f"load {os.getloadavg()[0]:.2f} at the end; runs failed {len(FAILED)}: {FAILED or '-'}")
for (name, window_mib, arm, smallest), took in sorted(set_runs.items(), key=lambda kv: (kv[0][:3], kv[0][3] or 0)):
    say(f"set {name} window {window_mib or 'default'} {arm:6} smallest {'-' if smallest is None else smallest} "
        f"runs {len(took)}: run p50 {statistics.median(took):.2f}s [{min(took):.2f}, {max(took):.2f}]")
say("of each cell's rounds: the median of its p50s and p99s, and their ranges (the rounds' spread)")
for (arm, s, cache, ph), rows in sorted(got.items(), key=lambda kv: (kv[0][1], kv[0][3], kv[0][2], kv[0][0])):
    p50s = [r[1] for r in rows]
    p99s = [r[3] for r in rows]
    say(f"{label(s):>6} {ph:9} {cache:4} {arm:6} rounds {len(rows)} files {sum(r[0] for r in rows)}: "
        f"p50 {statistics.median(p50s):.1f} [{min(p50s):.1f}, {max(p50s):.1f}]  "
        f"p99 {statistics.median(p99s):.1f} [{min(p99s):.1f}, {max(p99s):.1f}]")
