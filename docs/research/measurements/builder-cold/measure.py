#!/usr/bin/env python3
"""What a builder holds cold (D133, D188: the plan "Builders give back what they hold cold
and unplug under memory pressure"), measured on real builds: builds/rust (a Rust crate,
cargo's registry and target as cache mounts), builds/npm (npm ci of a lockfile, npm's cache
mounted) and builds/apt (an apt-get install, apt's archives and lists mounted), each built
--no-cache in rounds, the builds' order turning each round, by a build of shards with
cold.patch: its builder's console on the build's stderr, where its init writes a
`builder-cold` line each second and one at each step's end: its build cgroup's
memory.current, memory.peak, memory.stat's file, active_file, inactive_file, anon and
shmem, its swap, what the guest has (MemTotal, MemFree) of what it booted with and has
plugged, what it asked to have and the most it may, and what of what is plugged lies past
twice what it uses (in whole memory blocks, as a run's give-back keeps twice its use,
init's `to_keep`). Each line is stamped with this host's time as it arrives.

Beside it, each second, the builder's VM process on this host (the build's descendant
named shards-vm): its phys_footprint on macOS (proc_pid_rusage), its Pss, Rss and RssFile,
RssAnon and RssShmem on Linux (smaps_rollup, status).

    measure.py SHARDS OUT [--builds rust,npm,apt] [--rounds 3] [--home HOME]

Each build runs in HOME (default OUT/home, kept), with an empty DOCKER_CONFIG; a round's
first build of each kind fills its cache mounts, its later ones read them back. Into
OUT/out.txt: a header (host, OS, kernel, CPU, revision, memory), then `guest`, `host` and
`build` lines, KEY=VALUE each, for tables.py; into OUT/raw/, each build's stderr.
"""
import argparse, ctypes, os, platform, re, subprocess, sys, threading, time

here = os.path.dirname(os.path.realpath(__file__))
p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("out")
p.add_argument("--builds", default="rust,npm,apt")
p.add_argument("--rounds", type=int, default=3)
p.add_argument("--home")
a = p.parse_args()

shards = os.path.realpath(a.shards)
out = os.path.realpath(a.out)
os.makedirs(os.path.join(out, "raw"), exist_ok=True)
home = os.path.realpath(a.home) if a.home else os.path.join(out, "home")
config = os.path.join(out, "docker-config")
os.makedirs(config, exist_ok=True)
OUT = open(os.path.join(out, "out.txt"), "a")
macos = sys.platform == "darwin"


def say(line):
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
    say(f"builds {a.builds}; rounds {a.rounds}; home {home}")


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
        return f"footprint={info.phys_footprint} resident={info.resident_size}"
    try:
        fields = {}
        for line in open(f"/proc/{pid}/smaps_rollup"):
            w = line.split()
            if w and w[0] in ("Rss:", "Pss:", "Pss_File:", "Pss_Anon:", "Pss_Shmem:"):
                fields[w[0][:-1].lower()] = int(w[1]) << 10
        for line in open(f"/proc/{pid}/status"):
            w = line.split()
            if w and w[0] in ("RssFile:", "RssAnon:", "RssShmem:"):
                fields[w[0][:-1].lower()] = int(w[1]) << 10
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


def sample_host(build_pid, tag, stop):
    """The builder's VM process's memory each second, until `stop`."""
    while not stop.wait(1):
        vm = next((pid for pid, name in descendants(build_pid) if name == "shards-vm"), None)
        if vm is None:
            continue
        got = footprint(vm)
        if got:
            say(f"host t={time.time():.3f} {tag} pid={vm} {got}")


GUEST = re.compile(r"builder-cold (.*)$")
STEP = re.compile(r"^#(\d+) \[")


def one(r, name):
    tag = f"round={r} build={name}"
    ctx = os.path.join(here, "builds", name)
    env = dict(os.environ, SHARDS_HOME=home, DOCKER_CONFIG=config)
    started = time.time()
    raw = open(os.path.join(out, "raw", f"{r}-{name}.txt"), "w")
    # Its output and its progress one stream, as each line comes, wherever shards writes it.
    proc = subprocess.Popen([shards, "build", "--no-cache", "--progress", "plain", "-t", f"builder-cold-{name}",
                             "-f", os.path.join(ctx, "Dockerfile"), ctx], env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, errors="replace")
    stop = threading.Event()
    sampler = threading.Thread(target=sample_host, args=(proc.pid, tag, stop))
    sampler.start()
    step = "-"
    for line in proc.stdout:
        now = time.time()
        raw.write(f"{now:.3f} {line}")
        m = STEP.match(line)
        if m:
            step = m.group(1)
        m = GUEST.search(line)
        if m:
            words = m.group(1).split()
            end = words[-1] == "step_end"
            pairs = words[:-1] if end else words
            fields = " ".join(f"{k}={v}" for k, v in zip(pairs[::2], pairs[1::2]))
            say(f"guest t={now:.3f} {tag} step={step} step_end={int(end)} {fields}")
    code = proc.wait()
    stop.set()
    sampler.join()
    raw.close()
    say(f"build {tag} exit={code} s={time.time() - started:.1f}")


header()
builds = a.builds.split(",")
for r in range(1, a.rounds + 1):
    order = builds[(r - 1) % len(builds):] + builds[:(r - 1) % len(builds)]
    for name in order:
        one(r, name)
say("done")
