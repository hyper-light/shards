#!/usr/bin/env python3
"""Where a cold boot's time goes, shards against Firecracker, on one KVM host: the guest
the Firecracker comparison boots (crates/shards/benches/firecracker.rs: the pinned kernel,
the test guest in `idle` mode, the same command line), each VMM's boots interleaved.

1. VM exits: each boot under `perf kvm stat record`, to its ready line; `perf kvm stat
   report` per VMM: each exit reason's count and time, summed over the boots; and the
   first boot's exits in order (`perf script`), each with its time and guest address.
2. The guest's own timeline: one boot each with the kernel's initcalls timed
   (`initcall_debug`), `quiet`, so that no console line costs them anything, the test
   guest in `kmsg` mode printing the kernel's log once booted; each initcall's time,
   shards less Firecracker, the largest differences first.
3. shards' own: one boot with its log at debug, whose lines carry the VM process's
   microseconds since it started.
4. Before the guest runs: each boot's time from the VMM's exec to its vCPU's first entry,
   both events in one record (`perf record` of `sched:sched_process_exec` and
   `kvm:kvm_entry`), interleaved; and one boot each under `perf trace`, every system call
   in order with its time, thread and duration, to the guest's ready line.

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
# initcall_debug's lines, into a log large enough to hold them all, read from /dev/kmsg.
TIMED = "console=ttyS0 quiet panic=-1 initcall_debug log_buf_len=4M shards_test=kmsg"


def command(vmm, cmdline, name):
    if vmm == "shards":
        return [shards_vm, "run", "--kernel", kernel, "--initrd", initrd, "--cpus", "1",
                "--memory", "128", "--cmdline", cmdline]
    config = os.path.join(out, name)
    with open(config, "w") as f:
        json.dump({
            "boot-source": {"kernel_image_path": kernel, "initrd_path": initrd,
                            "boot_args": f"{cmdline} reboot=k pci=off"},
            "drives": [],
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
    # The VMM itself, whose command line starts with its path, not the wrapper, whose
    # holds it too: perf ends as its command does, and writes its record then.
    subprocess.run(["sudo", "pkill", "-KILL", "-f", "^" + re.escape(argv[0])], check=False)
    lines.extend(raw.decode(errors="replace") for raw in p.stdout)
    p.wait()
    with open(console, "w") as f:
        f.writelines(lines)
    if ready is None:
        print(f"{argv[0]}: no ready line; its last output:")
        print("".join(lines[-30:]))
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
            r = subprocess.run(["sudo", "perf", "kvm", "-i", data, "stat", "report", f"--event={event}",
                                "--key=time"], capture_output=True, text=True)
            reports.append(r.stdout + r.stderr)
        with open(os.path.join(out, f"{vmm}-{event}.txt"), "w") as f:
            f.write("\n".join(reports))
        print(f"== {vmm} {event}, boot 0")
        print(reports[0])

# 1b. One boot's exits in order, each VMM's: when each came (perf's clock), its reason,
# and the guest's address (kvm_exit's rip and, for a nested page fault, info2, the
# guest-physical address it faulted at), for the timeline of a boot before its guest
# keeps time.
for vmm in ["shards", "firecracker"]:
    data = os.path.join(out, vmm, "perf-0.data")
    r = subprocess.run(["sudo", "perf", "script", "-i", data, "-F", "time,event,trace"],
                       capture_output=True, text=True)
    with open(os.path.join(out, f"{vmm}-script-0.txt"), "w") as f:
        f.write(r.stdout + r.stderr)
    print(f"{vmm}: {len(r.stdout.splitlines())} events in boot 0's script")
# The kernel's symbols, to name the guest addresses the exits came from.
with open(os.path.join(out, "System.map"), "w") as f:
    subprocess.run(["nm", "-n", kernel], stdout=f, check=False)

# 2. The guest's initcalls, each VMM's guest run to its end.
initcalls = {}
for vmm in ["shards", "firecracker"]:
    console = os.path.join(out, f"{vmm}-initcalls.txt")
    with open(console, "w") as f:
        try:
            subprocess.run(command(vmm, TIMED, "fc-initcalls.json"), stdout=f, stderr=subprocess.STDOUT,
                           timeout=60)
        except subprocess.TimeoutExpired:
            print(f"{vmm}: its guest did not end in 60 s")
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

# 4. Before the guest runs: from the VMM's exec to its vCPU's first entry, perf's clock
# for both events, the boots interleaved.
entries = {"shards": [], "firecracker": []}
for i in range(boots):
    order = ["shards", "firecracker"] if i % 2 == 0 else ["firecracker", "shards"]
    for vmm in order:
        data = os.path.join(out, vmm, f"entry-{i}.data")
        boot(command(vmm, QUIET, f"fc-{i}.json"), os.path.join(out, vmm, f"entry-console-{i}.txt"),
             ["sudo", "perf", "record", "-q", "-o", data, "-e", "sched:sched_process_exec",
              "-e", "kvm:kvm_entry", "--"])
        r = subprocess.run(["sudo", "perf", "script", "-i", data, "-F", "time,event"],
                           capture_output=True, text=True)
        seen = re.findall(r"^\s*(\d+\.\d+):\s+(sched:sched_process_exec|kvm:kvm_entry):",
                          r.stdout, re.M)
        execs = [float(t) for t, e in seen if e == "sched:sched_process_exec"]
        first = [float(t) for t, e in seen if e == "kvm:kvm_entry"]
        if execs and first:
            entries[vmm].append(round((first[0] - execs[0]) * 1e6))
        else:
            print(f"{vmm}, boot {i}: no exec or no entry recorded\n{r.stdout[-1000:]}{r.stderr[-1000:]}")
print("exec to first entry, us:", json.dumps(entries))
for vmm, v in entries.items():
    if v:
        v = sorted(v)
        print(f"  {vmm}: n {len(v)}, p50 {v[len(v) // 2]}, max {v[-1]}")
if len(entries["shards"]) == len(entries["firecracker"]) == boots:
    d = sorted(a - b for a, b in zip(entries["shards"], entries["firecracker"]))
    print(f"  shards less Firecracker, paired: p50 {d[boots // 2]}, min {d[0]}, max {d[-1]}")

# 4b. One boot each under perf trace: the system calls before the guest ran, and after.
for vmm in ["shards", "firecracker"]:
    trace = os.path.join(out, f"{vmm}-syscalls.txt")
    boot(command(vmm, QUIET, "fc-0.json"), os.path.join(out, vmm, "trace-console.txt"),
         ["sudo", "perf", "trace", "-T", "-o", trace, "--"])
    lines = open(trace, errors="replace").read().splitlines() if os.path.exists(trace) else []
    run = next((n for n, l in enumerate(lines) if "KVM_RUN" in l), None)
    print(f"{vmm}: {len(lines)} lines traced; the first KVM_RUN at line {run}")
    for l in lines[:3] + (lines[max(run - 3, 0):run + 1] if run is not None else []):
        print("   ", l[:200])
