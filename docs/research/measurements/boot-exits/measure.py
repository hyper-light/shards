#!/usr/bin/env python3
"""Where a cold boot's time goes, shards against Firecracker, on one KVM host: the guest
the Firecracker comparison boots (crates/shards/benches/firecracker.rs: the pinned kernel,
the test guest in `idle` mode, the same command line), each VMM's boots interleaved.

1. VM exits: each boot under `perf kvm stat record`, to its ready line; `perf kvm stat
   report` per VMM: each exit reason's count and time, summed over the boots.
2. The guest's own timeline: one boot each with the kernel's initcalls timed
   (`initcall_debug`, not `quiet`), the console kept; each initcall's time, shards less
   Firecracker, the largest differences first.
3. shards' own: one boot with its log at debug, whose lines carry the VM process's
   microseconds since it started.

Run from the workspace after `cargo bench -p shards --bench firecracker -- --runs 1`,
which leaves the kernel, the initrd and Firecracker in target/artifacts and the VM process
among the helpers shards carries. Needs perf (linux-tools) and sudo.

    measure.py OUT_DIR [BOOTS]
"""
import glob, json, os, re, shutil, subprocess, sys, time

out = sys.argv[1]
boots = int(sys.argv[2]) if len(sys.argv) > 2 else 10
os.makedirs(out, exist_ok=True)


def one(pattern):
    found = sorted(glob.glob(pattern, recursive=True), key=os.path.getmtime)
    if not found:
        sys.exit(f"nothing matches {pattern}")
    return found[-1]


kernel = one("target/artifacts/vmlinux-*-x86_64-*")
initrd = one("target/artifacts/idle.initrd")
firecracker = one("target/artifacts/firecracker-*/release-*/firecracker-v*-x86_64")
shards_vm = one("target/release/build/shards-*/out/helpers/shards-vm")
print(f"kernel {kernel}\ninitrd {initrd}\nfirecracker {firecracker}\nshards-vm {shards_vm}")

QUIET = "console=ttyS0 quiet panic=-1 shards_test=idle"
VERBOSE = "console=ttyS0 printk.time=1 initcall_debug loglevel=7 panic=-1 shards_test=idle"


def command(vmm, cmdline, name):
    if vmm == "shards":
        return [shards_vm, "run", "--kernel", kernel, "--initrd", initrd, "--cpus", "1",
                "--memory", "128", "--cmdline", cmdline]
    config = os.path.join(out, name)
    with open(config, "w") as f:
        json.dump({
            "boot-source": {"kernel_image_path": kernel, "initrd_path": initrd,
                            "boot_args": f"{cmdline} reboot=k pci=off"},
            "machine-config": {"vcpu_count": 1, "mem_size_mib": 128},
        }, f)
    return [firecracker, "--no-api", "--config-file", config, "--level", "error"]


def boot(argv, console, wrap=None, env=None):
    """Runs `argv` (under `wrap`) until the guest's ready line, keeping its console and its
    log; kills it then. The ready line is the test guest's (testguest `idle`)."""
    start = time.monotonic()
    p = subprocess.Popen((wrap or []) + argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         env=env)
    lines = []
    ready = None
    for raw in p.stdout:
        line = raw.decode(errors="replace")
        lines.append(line)
        if "SHARDS-TEST READY" in line:
            ready = time.monotonic() - start
            break
    # The VMM itself, not the wrapper: perf ends as its command does.
    subprocess.run(["sudo", "pkill", "-KILL", "-f", argv[0]], check=False)
    p.wait()
    with open(console, "w") as f:
        f.writelines(lines)
    return ready


# 1. Exits.
for vmm in ["shards", "firecracker"]:
    os.makedirs(os.path.join(out, vmm), exist_ok=True)
times = {"shards": [], "firecracker": []}
for i in range(boots):
    order = ["shards", "firecracker"] if i % 2 == 0 else ["firecracker", "shards"]
    for vmm in order:
        data = os.path.join(out, vmm, f"perf-{i}.data")
        ready = boot(command(vmm, QUIET, f"fc-{i}.json"), os.path.join(out, vmm, f"console-{i}.txt"),
                     ["sudo", "perf", "kvm", "stat", "record", "-q", "-o", data, "--"])
        times[vmm].append(ready)
print("to ready under perf, s:", json.dumps(times))
for vmm in ["shards", "firecracker"]:
    for event in ["vmexit", "ioport", "mmio"]:
        reports = []
        for i in range(boots):
            data = os.path.join(out, vmm, f"perf-{i}.data")
            r = subprocess.run(["sudo", "perf", "kvm", "stat", "report", "-i", data, f"--event={event}",
                                "--key=time"], capture_output=True, text=True)
            reports.append(r.stdout)
        with open(os.path.join(out, f"{vmm}-{event}.txt"), "w") as f:
            f.write("\n".join(reports))
        print(f"== {vmm} {event}, boot 0")
        print(reports[0])

# 2. The guest's initcalls.
initcalls = {}
for vmm in ["shards", "firecracker"]:
    console = os.path.join(out, f"{vmm}-verbose.txt")
    boot(command(vmm, VERBOSE, "fc-verbose.json"), console)
    calls = {}
    for line in open(console, errors="replace"):
        m = re.search(r"initcall (\S+) returned \S+ after (\d+) usecs", line)
        if m:
            calls[m.group(1)] = calls.get(m.group(1), 0) + int(m.group(2))
    initcalls[vmm] = calls
    print(f"{vmm}: {len(calls)} initcalls, {sum(calls.values())} us in them")
names = set(initcalls["shards"]) | set(initcalls["firecracker"])
diffs = sorted(((initcalls["shards"].get(n, 0) - initcalls["firecracker"].get(n, 0), n) for n in names),
               key=lambda d: -abs(d[0]))
print("initcalls, shards less Firecracker, us (largest first):")
for d, n in diffs[:40]:
    print(f"  {d:+8d}  {n}  (shards {initcalls['shards'].get(n, 0)}, Firecracker {initcalls['firecracker'].get(n, 0)})")

# 3. shards' own timeline.
log = os.path.join(out, "shards-debug.txt")
boot(command("shards", QUIET, ""), log, env=dict(os.environ, SHARDS_LOG="debug"))
print("shards at debug:")
print(open(log, errors="replace").read())
