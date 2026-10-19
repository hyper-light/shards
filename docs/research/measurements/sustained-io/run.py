#!/usr/bin/env python3
"""Sustained I/O: the long workloads of an agent's session on shards, Docker, Firecracker
and Cloud Hypervisor (docs/research/platform-measurements.md M266). Each run is a fresh
microVM or container of 8 vCPUs and 8 GiB on the host's CPUs 0-15 (the i9-14900HX's
P-cores, M258), the systems taking turns in an order that rotates each round, so that
what the host is doing then falls on each alike. Its workloads (work.sh):

- nop: `true`, a microVM's or container's start and end alone;
- npm: `npm ci --offline` of npm/cli v12.2.1 (1,202 packages) from a cache in the image;
- cargo: `cargo build --release --offline -p rust-analyzer` of rust-analyzer 2026-10-19,
  its dependencies vendored;
- test: that tree's `cargo test --offline --workspace`, xtask aside (it reads git),
  smol_str's check_code_formatting (it runs rustfmt, which the image has not) and
  project-model's smoke_test_real_sysroot_cargo (rustup fetches the standard library's
  sources for it: it fails without a network, as every system's run has none, Docker's
  given `--network none` as the microVMs have no device);
- fio: fio.ini's jobs, each 20 s after 3 s of ramp;

each where it writes: the root's writable layer (layer), a directory of the host's given
in (volume), and fio a disk given whole (block):

    system            layer                         volume                       block
    shards            overlay over EROFS, tmpfs     virtio-fs, shards' server    -
    shards-vmm        ext4 root on virtio-blk       ext4 on a second disk        virtio-blk
    docker            overlayfs, host filesystem    bind mount                   -
    firecracker       ext4 root on virtio-blk       ext4 on a second disk        virtio-blk
    cloud-hypervisor  ext4 root on virtio-blk       virtio-fs, virtiofsd         virtio-blk

`shards` is a run as its users make one (`shards run`: its daemon, pool and template);
`shards-vmm` is shards' VMM booting the image's ext4 root as Firecracker and Cloud
Hypervisor boot it (`shards run --kernel`, init.sh as PID 1), the same kernel in all
three (shards' pinned 6.18.48), so that the VMMs compare apart from shards' runtime. Each
system's devices are as it ships them: Firecracker's drives its Sync engine, Cloud
Hypervisor's disks without direct I/O, virtiofsd with cache=auto (its memory then
shared=on, as vhost-user needs).

Per run: the host's wall time from the request to the exit (total), the workload's own
(work), and what the run used of the host: its cgroup's memory.peak and CPU time (the
VMM's transient scope, virtiofsd's with it; shards' daemon's scope, its pool's microVMs
with it, the peak reset before each run, and what it held before it; the container's own
cgroup), and of a microVM, its VM process's own peak resident set (wait4's ru_maxrss of
the VMM; a `shards run`'s VM process's VmHWM, read until it ends). And fio's IOPS,
bandwidth and completion latencies, per job.

    run.py SHARDS ROUNDS [SYSTEM,...] [WORKLOAD:PLACE,...]   appends to $SIO/results.jsonl
    run.py report [FILE]

SIO as fetch.sh's, after fetch.sh and images.sh. Fedora's host needs no root: the VMMs
run as its user, in scopes of its systemd (`systemd-run --user`).
"""
import json, math, os, re, shutil, socket, subprocess, sys, threading, time
from pathlib import Path

SIO = Path(os.environ.get("SIO", Path.home() / "sio"))
HERE = Path(__file__).resolve().parent
CPUS, MEMORY_MIB, HOST_CPUS = 8, 8192, "0-15"
TIMEOUT = 3600
IMAGE = "shards-sio:1"
SYSTEMS = ["shards", "shards-vmm", "docker", "firecracker", "cloud-hypervisor"]
CELLS = [("nop", "layer"), ("npm", "layer"), ("npm", "volume"), ("cargo", "layer"),
         ("cargo", "volume"), ("test", "layer"), ("fio", "layer"), ("fio", "volume"),
         ("fio", "block")]
BLOCKLESS = {"shards", "docker"}
TREES = {"npm": "cli", "cargo": "rust-analyzer", "test": "rust-analyzer"}
BOOT = "console=ttyS0 quiet panic=1 root=/dev/vda rw init=/sio/init"


def pct(v, p):
    v = sorted(v)
    return v[min(max(1, math.ceil(p / 100 * len(v))), len(v)) - 1]


def env():
    e = dict(os.environ, SHARDS_HOME=str(SIO / "home"), DOCKER_CONFIG=str(SIO / "docker-config"))
    return e


def cgroup_of(pid="self"):
    line = Path(f"/proc/{pid}/cgroup").read_text().strip().split("\n")[-1]
    return Path("/sys/fs/cgroup" + line.split("::", 1)[1])


def cpu_usec(cg):
    for line in (cg / "cpu.stat").read_text().splitlines():
        k, v = line.split()
        if k == "usage_usec":
            return int(v)
    return 0


def parse(out):
    """The SIO line's measures, and fio's report, from a run's output."""
    lines = out.replace("\r", "").split("\n")
    got = {}
    # Each marker wherever its line has it: a program before may have left a line
    # unended (npm's progress).
    for line in lines:
        if m := re.search(r"SIO-STATUS (-?\d+)", line):
            got["guest_status"] = int(m.group(1))
        elif m := re.search(r"\bSIO(?:-HOST)? ((?:\w+=\S+ ?)+)$", line):
            for kv in m.group(1).split():
                k, _, v = kv.partition("=")
                got[k] = int(v) if v.lstrip("-").isdigit() else v
    begin = [i for i, line in enumerate(lines) if line.endswith("SIO-FIO-BEGIN")]
    end = [i for i, line in enumerate(lines) if line.endswith("SIO-FIO-END")]
    if begin and end:
        text = "\n".join(lines[begin[0] + 1:end[-1]])
        try:
            report = json.loads(text[text.index("{"):])
        except ValueError as e:
            got["fio_error"] = str(e)
        else:
            got["fio"] = {j["jobname"]: job(j) for j in report.get("jobs", [])}
    return got


def job(j):
    side = j["write"] if j["write"]["io_bytes"] > j["read"]["io_bytes"] else j["read"]
    clat = side.get("clat_ns", {})
    pcts = clat.get("percentile", {})
    return {"error": j.get("error", 0), "iops": side["iops"], "bw_kib": side["bw"],
            "clat_p50_us": pcts.get("50.000000", 0) / 1e3,
            "clat_p99_us": pcts.get("99.000000", 0) / 1e3,
            "clat_p999_us": pcts.get("99.900000", 0) / 1e3,
            "clat_max_us": clat.get("max", 0) / 1e3}


def scope(spec_path):
    """Inside a transient scope: start the helpers, then the VMM; say the scope's use."""
    spec = json.loads(Path(spec_path).read_text())
    cg = cgroup_of()
    start = time.monotonic_ns()
    helpers = []
    for h in spec.get("helpers", []):
        log = open(h["log"], "w")
        helpers.append(subprocess.Popen(h["argv"], stdout=log, stderr=subprocess.STDOUT))
        deadline = time.monotonic() + 30
        while not Path(h["socket"]).exists():
            if time.monotonic() > deadline or helpers[-1].poll() is not None:
                print(f"SIO-HOST helper_failed={h['argv'][0]}", flush=True)
                return 1
            time.sleep(0.005)
    # The VMM, waited on with wait4(2): its own peak resident set (ru_maxrss, KiB).
    vmm = subprocess.Popen(spec["argv"])
    timer = threading.Timer(TIMEOUT, vmm.kill)
    timer.start()
    _, raw, usage = os.wait4(vmm.pid, 0)
    timer.cancel()
    vmm.returncode = os.waitstatus_to_exitcode(raw)
    total = time.monotonic_ns() - start
    for p in helpers:
        p.terminate()
        p.wait()
    peak = int((cg / "memory.peak").read_text())
    print(f"SIO-HOST total_ns={total} host_peak={peak} host_cpu_usec={cpu_usec(cg)} "
          f"vmm_hwm={usage.ru_maxrss * 1024} status={vmm.returncode}", flush=True)
    return 0


def fresh(path):
    """An empty run directory: the last run's removed (under SIO/runs alone)."""
    runs = (SIO / "runs").resolve()
    path = path.resolve()
    if runs not in path.parents:
        raise SystemExit(f"{path} is not under {runs}")
    if path.exists():
        try:
            shutil.rmtree(path)
        except PermissionError:
            # A container's root left files there (a Docker volume's): its own to remove.
            subprocess.run(["docker", "run", "--rm", "-v", f"{path}:/r", IMAGE, "sh", "-c",
                            "rm -rf /r/* /r/.[!.]*"], env=env(), capture_output=True)
            shutil.rmtree(path)
    path.mkdir(parents=True)
    return path


def mke2fs(image, size, source=None):
    with open(image, "wb") as f:
        f.truncate(size)
    cmd = ["mke2fs", "-q", "-t", "ext4", "-F"] + (["-d", str(source)] if source else []) + [str(image)]
    subprocess.run(cmd, check=True)


class Shards:
    """shards' daemon, in a scope of its own on the benchmark's CPUs, kept for the whole
    benchmark: a round's other systems take longer than the daemon's idle exit
    (SHARDS_DAEMON_IDLE, 900 s), which the first run of the 2026-10-19 benchmark met."""

    def __init__(self, shards):
        self.shards = shards
        self.starts = 0
        subprocess.run([shards, "daemon", "stop"], env=env(), capture_output=True)
        self.start()

    def start(self):
        self.starts += 1
        self.unit = f"sio-shards-{os.getpid()}-{self.starts}.scope"
        log = open(SIO / "daemon-run.log", "a")
        self.daemon = subprocess.Popen(
            ["systemd-run", "--user", "--scope", "-q", f"--unit={self.unit}",
             f"-p", f"AllowedCPUs={HOST_CPUS}", self.shards, "daemon"],
            env=dict(env(), SHARDS_DAEMON_IDLE=str(7 * 86400)), stdout=log,
            stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 30
        while True:
            show = subprocess.run(["systemctl", "--user", "show", "-P", "ControlGroup", self.unit],
                                  capture_output=True, text=True).stdout.strip()
            if show:
                self.cg = Path("/sys/fs/cgroup" + show)
                break
            if time.monotonic() > deadline:
                raise SystemExit("shards' daemon's scope did not start")
            time.sleep(0.05)

    def ensure(self):
        """The daemon running: started again, and said, where it has ended."""
        if self.daemon.poll() is None and (self.cg / "memory.peak").exists():
            return
        print(f"shards' daemon ended (status {self.daemon.poll()}); starting it again; "
              f"its log: {SIO / 'daemon-run.log'}", flush=True)
        self.start()

    def stop(self):
        subprocess.run([self.shards, "daemon", "stop"], env=env(), capture_output=True)
        self.daemon.wait(timeout=60)


def command(system, workload, place, run, shards, daemon):
    """The run's argv, how its host measures are taken, and its spec for a scope."""
    work = ["/sio/work", workload, place]
    if system in ("shards", "docker"):
        argv = [shards if system == "shards" else "docker", "run", "--rm", "--cpus", str(CPUS),
                "-m", f"{MEMORY_MIB}m", "--memory-swap", f"{MEMORY_MIB}m"]
        argv += ["--name", f"sio-{os.getpid()}"]
        if system == "docker":
            argv += ["--cpuset-cpus", HOST_CPUS, "--network", "none"]
        if place == "volume":
            argv += ["-v", f"{run / 'vol'}:/vol"]
        return argv + [IMAGE] + work, None
    kernel, root = SIO / "bin" / "kernel", run / "rootfs.ext4"
    second = {"volume": run / "vol.ext4", "block": run / "data.img"}.get(place)
    vol = {"volume": "disk"}.get(place, "none")
    if system == "cloud-hypervisor" and place == "volume":
        vol, second = "fs", None
    args = f"{BOOT} sio.workload={workload} sio.place={place} sio.vol={vol}"
    spec = {"helpers": []}
    if system == "firecracker":
        config = {
            "boot-source": {"kernel_image_path": str(kernel),
                            "boot_args": f"{args} sio.off=reboot reboot=k pci=off"},
            "drives": [{"drive_id": "root", "path_on_host": str(root), "is_root_device": True,
                        "is_read_only": False}]
            + ([{"drive_id": "second", "path_on_host": str(second), "is_root_device": False,
                 "is_read_only": False}] if second else []),
            "machine-config": {"vcpu_count": CPUS, "mem_size_mib": MEMORY_MIB},
        }
        (run / "fc.json").write_text(json.dumps(config))
        spec["argv"] = [str(SIO / "bin" / "firecracker"), "--no-api", "--config-file",
                        str(run / "fc.json")]
    elif system == "cloud-hypervisor":
        memory = f"size={MEMORY_MIB}M" + (",shared=on" if vol == "fs" else "")
        disks = [f"path={p},image_type=raw" for p in [root] + ([second] if second else [])]
        spec["argv"] = [str(SIO / "bin" / "cloud-hypervisor"), "--kernel", str(kernel),
                        "--cmdline", args, "--cpus", f"boot={CPUS}", "--memory", memory,
                        "--disk", *disks, "--serial", "tty", "--console", "off"]
        if vol == "fs":
            sock = run / "vfsd.sock"
            spec["helpers"].append({
                "argv": [str(SIO / "bin" / "virtiofsd"), f"--socket-path={sock}",
                         f"--shared-dir={run / 'vol'}", "--cache=auto", "--sandbox=none"],
                "socket": str(sock), "log": str(run / "virtiofsd.log")})
            spec["argv"] += ["--fs", f"tag=vol,socket={sock}"]
    else:
        spec["argv"] = [shards, "run", "--kernel", str(kernel), "--cmdline", args,
                        "--cpus", str(CPUS), "--memory", str(MEMORY_MIB), "--disk", str(root)] \
            + (["--disk", str(second)] if second else [])
    (run / "scope.json").write_text(json.dumps(spec))
    argv = ["systemd-run", "--user", "--scope", "-q", "-p", f"AllowedCPUs={HOST_CPUS}",
            sys.executable, "-I", str(Path(__file__).resolve()), "scope", str(run / "scope.json")]
    return argv, spec


def prepare(system, workload, place, run):
    if system not in ("shards", "docker"):
        subprocess.run(["cp", "--reflink=auto", str(SIO / "rootfs.ext4"), str(run / "rootfs.ext4")],
                       check=True)
    if place == "volume":
        vol = run / "vol"
        vol.mkdir()
        if workload in TREES:
            tree = TREES[workload]
            subprocess.run(["cp", "-a", "--reflink=auto", str(SIO / "ctx" / tree), str(vol / tree)],
                           check=True)
        if system in ("shards-vmm", "firecracker"):
            mke2fs(run / "vol.ext4", 16 << 30, vol)
    if place == "block":
        with open(run / "data.img", "wb") as f:
            f.truncate(8 << 30)


def once(system, workload, place, rnd, shards, daemon):
    run = fresh(SIO / "runs" / "current")
    prepare(system, workload, place, run)
    argv, spec = command(system, workload, place, run, shards, daemon)
    peak_fd, cpu0, base = None, 0, 0
    if system == "shards":
        daemon.ensure()
        peak_fd = os.open(daemon.cg / "memory.peak", os.O_RDWR)
        os.write(peak_fd, b"reset")
        cpu0, base = cpu_usec(daemon.cg), int((daemon.cg / "memory.current").read_text())
    hwm = {}
    watching = threading.Event()
    if system == "shards":
        # The run's VM process, as `inspect` names it, its high-water mark read until it
        # ends (VmHWM: what of the daemon's scope is this run's own VM's).
        def watch():
            pid = 0
            while not watching.is_set():
                if not pid:
                    got = subprocess.run([shards, "inspect", "-f", "{{.State.Pid}}", f"sio-{os.getpid()}"],
                                         env=env(), capture_output=True, text=True).stdout.strip()
                    pid = int(got) if got.isdigit() else 0
                try:
                    for line in Path(f"/proc/{pid}/status").read_text().splitlines() if pid else []:
                        if line.startswith("VmHWM:"):
                            hwm["kib"] = int(line.split()[1])
                except OSError:
                    pass
                watching.wait(0.2)
        watcher = threading.Thread(target=watch, daemon=True)
        watcher.start()
    start = time.monotonic_ns()
    try:
        r = subprocess.run(argv, env=env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           timeout=TIMEOUT + 60)
        out, status = r.stdout.decode(errors="replace"), r.returncode
    except subprocess.TimeoutExpired as e:
        out, status = (e.stdout or b"").decode(errors="replace"), -1
    total = time.monotonic_ns() - start
    watching.set()
    got = parse(out)
    for k in ("status", "guest_status"):
        if got.get(k, 0) != 0:
            status = got[k]
    row = {"round": rnd, "system": system, "workload": workload, "place": place,
           "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "status": status,
           "total_s": got.get("total_ns", total) / 1e9}
    if "work_ns" in got:
        row["work_s"] = got["work_ns"] / 1e9
    for k in ("guest_peak", "host_peak", "vmm_hwm"):
        if k in got:
            row[k + "_mib"] = got[k] / 2**20
    if "kib" in hwm:
        row["vmm_hwm_mib"] = hwm["kib"] / 1024
    if "guest_cpu_usec" in got:
        row["guest_cpu_s"] = got["guest_cpu_usec"] / 1e6
    if "host_cpu_usec" in got:
        row["host_cpu_s"] = got["host_cpu_usec"] / 1e6
    if system == "docker":
        # The container's own cgroup is the host's: what its workload saw it use.
        row["host_peak_mib"], row["host_cpu_s"] = row.get("guest_peak_mib"), row.get("guest_cpu_s")
    if peak_fd is not None:
        row["host_peak_mib"] = int(os.pread(peak_fd, 64, 0)) / 2**20
        row["host_base_mib"] = base / 2**20
        row["host_cpu_s"] = (cpu_usec(daemon.cg) - cpu0) / 1e6
        os.close(peak_fd)
    if "fio" in got:
        row["fio"] = got["fio"]
    if status != 0 or "work_s" not in row:
        row["error"] = out[-3000:]
    if system == "docker" and place == "volume" and (run / "vol").exists():
        # What its root wrote there is root's on the host: removed as it was written.
        subprocess.run(["docker", "run", "--rm", "-v", f"{run}:/r", IMAGE, "rm", "-rf", "/r/vol"],
                       env=env(), capture_output=True)
    return row


def rounds(shards, n, systems, cells):
    head = subprocess.run(["git", "-C", str(HERE), "rev-parse", "--short", "HEAD"],
                          capture_output=True, text=True).stdout.strip()
    version = subprocess.run([shards, "--version"], capture_output=True, text=True).stdout.strip()
    meta = {"host": socket.gethostname(), "kernel": os.uname().release, "rev": head,
            "shards": version}
    daemon = Shards(shards) if "shards" in systems else None
    try:
        # Unrecorded: each system's first start, which the rest do not pay again (shards'
        # template of the image, D25; the host's cache of the kernel and the root).
        for system in systems:
            row = once(system, "nop", "layer", -1, shards, daemon)
            print(f"warm-up {system:16} status {row['status']} total {row['total_s']:.2f} s", flush=True)
        with open(SIO / "results.jsonl", "a") as out:
            for rnd in range(n):
                order = systems[rnd % len(systems):] + systems[:rnd % len(systems)]
                for workload, place in cells:
                    for system in order:
                        if place == "block" and system in BLOCKLESS:
                            continue
                        row = dict(meta, **once(system, workload, place, rnd, shards, daemon))
                        out.write(json.dumps(row) + "\n")
                        out.flush()
                        print(f"{rnd} {system:16} {workload:5} {place:6} status {row['status']} "
                              f"total {row['total_s']:.2f} s work {row.get('work_s', 0):.2f} s",
                              flush=True)
    finally:
        if daemon:
            daemon.stop()


def report(path):
    rows = [json.loads(l) for l in open(path)]
    ok = [r for r in rows if r["status"] == 0 and "work_s" in r]
    print(f"{len(rows)} runs, {len(rows) - len(ok)} failed; hosts {sorted({r['host'] for r in rows})}, "
          f"kernels {sorted({r['kernel'] for r in rows})}, revisions {sorted({r['rev'] for r in rows})}")
    for r in rows:
        if r not in ok:
            print(f"  failed: round {r['round']} {r['system']} {r['workload']} {r['place']}: "
                  f"status {r['status']}: {r.get('error', '')[-300:]!r}")

    def line(label, v, unit):
        return (f"{label} n {len(v)} | p50 {pct(v, 50):.2f} | p90 {pct(v, 90):.2f} | "
                f"p99 {pct(v, 99):.2f} | max {max(v):.2f} {unit}")

    for workload, place in CELLS:
        cell = [r for r in ok if r["workload"] == workload and r["place"] == place]
        if not cell:
            continue
        print(f"\n{workload} on {place}")
        for system in SYSTEMS:
            mine = [r for r in cell if r["system"] == system]
            if not mine:
                continue
            print(f"  {system:16} " + line("total", [r["total_s"] for r in mine], "s"))
            print(f"  {'':16} " + line("work ", [r["work_s"] for r in mine], "s"))
            peaks = [r["host_peak_mib"] for r in mine if r.get("host_peak_mib") is not None]
            above = [r["host_peak_mib"] - r["host_base_mib"] for r in mine if "host_base_mib" in r]
            hwm = [r["vmm_hwm_mib"] for r in mine if r.get("vmm_hwm_mib") is not None]
            cpu = [r["host_cpu_s"] for r in mine if r.get("host_cpu_s") is not None]
            if peaks:
                print(f"  {'':16} " + line("host memory peak", peaks, "MiB"))
            if above:
                print(f"  {'':16} " + line("  above its base", above, "MiB"))
            if hwm:
                print(f"  {'':16} " + line("VM process peak", hwm, "MiB"))
            if cpu:
                print(f"  {'':16} " + line("host CPU", cpu, "s"))
            if workload == "fio":
                jobs = list(mine[0].get("fio", {}))
                for name in jobs:
                    js = [r["fio"][name] for r in mine if name in r.get("fio", {})]
                    good = [j for j in js if not j["error"]]
                    if len(good) < len(js):
                        print(f"  {'':16} {name}: error {sorted({j['error'] for j in js})}")
                    if not good:
                        continue
                    key, unit = ("bw_kib", "MiB/s") if "1m" in name else ("iops", "IOPS")
                    v = [j[key] / (1024 if key == "bw_kib" else 1) for j in good]
                    print(f"  {'':16} " + line(f"{name:18}", v, unit)
                          + f" | clat p50 {pct([j['clat_p50_us'] for j in good], 50):.0f}"
                          + f" p99 {pct([j['clat_p99_us'] for j in good], 50):.0f}"
                          + f" max {max(j['clat_max_us'] for j in good):.0f} us")


if __name__ == "__main__":
    a = sys.argv[1:]
    if a[:1] == ["scope"]:
        sys.exit(scope(a[1]))
    if a[:1] == ["report"]:
        report(a[1] if len(a) > 1 else SIO / "results.jsonl")
        sys.exit(0)
    systems = a[2].split(",") if len(a) > 2 and a[2] else SYSTEMS
    cells = [tuple(c.split(":")) for c in a[3].split(",")] if len(a) > 3 else CELLS
    rounds(str(Path(a[0]).resolve()), int(a[1]), systems, cells)
