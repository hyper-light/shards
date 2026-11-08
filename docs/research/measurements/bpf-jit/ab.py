#!/usr/bin/env python3
"""PM M462 (D215): the guest's BPF programs, interpreted against JIT-compiled. Two guest
kernels booted in turn on this host, A then B, then B then A, and so on, each boot one
round of the test guest's `bpf` mode (crates/testguest/src/linux.rs `bpf_costs`):

    ab.py --shards SHARDS --guest TESTGUEST --a KERNEL --b KERNEL
          [--rounds 10] [--n 10000] [--batch 16] [--loads 10000]
          [--cpus 1] [--memory 512] [--out FILE.json]

SHARDS is the `shards` that boots each (`run --kernel`; on macOS a copy carrying its
signed VM process, as the E2E tests' target/e2e/shards). TESTGUEST is the test guest built
for the guest (static musl, `--profile guest`), PID 1 of each boot. A is the kernel pinned
now (src/kernel.rs, BPF_JIT unset), B the one built with resources/kernel/shards.config's
BPF_JIT and BPF_JIT_ALWAYS_ON. Each boot's record goes to a disk of its own (`--disk`,
/dev/vda in the guest), read after it powers off.

In each round every arm is a child of the guest's PID 1, timing --n batches of --batch
calls (the clock read once a batch, after n/10 untimed batches), so each sample is a batch's
time over its calls:

    clock               an empty batch: the clock's own cost
    none/getppid        getppid(2) with no filter
    none/personality    personality(0xffffffff) with no filter
    root/getppid        getppid(2) under Docker's default profile, loaded with init's
                        capabilities (a run without no-new-privileges): seccomp's action
                        cache may answer it without running the filter
    root/personality    personality(0xffffffff), allowed by its argument: the filter runs
    root/denied         acct(NULL), refused (EPERM): the filter's whole path
    nnp/*               the same, the filter loaded as user 65534 with no capabilities
                        and no_new_privs (a run with no-new-privileges)
    dev/open            open(2)+close(2) of /dev/null in a cgroup with Docker's default
                        device filter (D44)
    nodev/open          the same in a cgroup with no device filter
    root/load, nnp/load one seccomp(2) load of the profile into a fresh child, --loads of
                        them a round, the time of the call alone
    device/load         the device filter's load and attach, once a boot

Reports per arm and kernel n, p50, p90, p99 and max in ns per call, pooled over the rounds,
and the spread of the rounds' p50s; the guest's MemAvailable at boot, after the device
filter, after the arms and at the end (KiB, p50 over rounds), and the vmalloc areas of the
JIT. On macOS run it as a user's program, outside an agent session's QoS clamp:
docs/research/measurements/macos-sched/launchd-run.sh OUT python3 ab.py ...
"""

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile

WARMUP = 1  # boots of each kernel before the rounds that count


def percentile(v, p):
    # The nearest rank, as crates/shards/benches/support and kernel-ab/ab.py.
    rank = max(1, math.ceil(p / 100 * len(v)))
    return v[min(rank, len(v)) - 1]


def parse(text):
    rec = {"arms": {}, "mem": {}, "vuln": {}, "vmalloc": {}}
    ended = False
    for line in text.splitlines():
        word, _, rest = line.partition(" ")
        if word == "arm":
            name, batch, *values = rest.split()
            rec["arms"][name] = (int(batch), [int(v) for v in values])
        elif word == "mem":
            at, free, avail = rest.split()
            rec["mem"][at] = (free, avail)
        elif word == "vuln":
            name, _, said = rest.partition(" ")
            rec["vuln"][name] = said
        elif word == "vmalloc":
            caller, size = rest.split()
            rec["vmalloc"][caller] = int(size)
        elif word in ("jit", "kernel", "filter", "device"):
            rec[word] = rest
        elif word == "end":
            ended = True
            break
    if not ended:
        raise SystemExit(f"a record cut short:\n{text[:2000]}")
    return rec


def boot(args, kernel, work, env):
    # Room for every sample as text, generously, in whole MiB.
    values = 12 * args.n + 2 * args.loads + 1
    size = max(4 << 20, (values * 12 + (1 << 20)) // (1 << 20) * (1 << 20))
    disk = os.path.join(work, "record")
    with open(disk, "wb") as f:
        f.truncate(size)
    cmdline = (
        f"console=ttyS0 quiet panic=-1 shards_test=bpf shards_bpf_n={args.n} "
        f"shards_bpf_batch={args.batch} shards_bpf_loads={args.loads}"
    )
    r = subprocess.run(
        [
            args.shards, "run", "--kernel", kernel, "--init", args.guest,
            "--cpus", str(args.cpus), "--memory", str(args.memory),
            "--disk", disk, "--cmdline", cmdline,
        ],
        env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=1800,
    )
    if r.returncode != 0 or "SHARDS-TEST PASS" not in r.stdout:
        raise SystemExit(
            f"{os.path.basename(kernel)}: exit {r.returncode}\n"
            f"--- stdout\n{r.stdout[-4000:]}\n--- stderr\n{r.stderr[-4000:]}"
        )
    with open(disk, "rb") as f:
        return parse(f.read().rstrip(b"\0").decode())


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:12]


def main():
    ap = argparse.ArgumentParser()
    for flag in ("--shards", "--guest", "--a", "--b"):
        ap.add_argument(flag, required=True)
    ap.add_argument("--rounds", type=int, default=10)
    ap.add_argument("--n", type=int, default=10_000)
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--loads", type=int, default=10_000)
    ap.add_argument("--cpus", type=int, default=1)
    ap.add_argument("--memory", type=int, default=512)
    ap.add_argument("--out")
    args = ap.parse_args()
    kernels = {"A": args.a, "B": args.b}
    work = tempfile.mkdtemp(prefix="bpf-jit-")
    # No docker config of this host's is read (the tests' rule for manual runs).
    os.makedirs(os.path.join(work, "docker"))
    env = dict(os.environ, DOCKER_CONFIG=os.path.join(work, "docker"))
    records = {k: [] for k in kernels}
    try:
        for i in range(WARMUP + args.rounds):
            for name in ("A", "B") if i % 2 == 0 else ("B", "A"):
                rec = boot(args, kernels[name], work, env)
                if i >= WARMUP:
                    records[name].append(rec)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    rev = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True,
        cwd=os.path.dirname(os.path.abspath(__file__)),
    ).stdout.strip()
    u = platform.uname()
    print(f"host {u.node} {u.machine} · {u.system} {u.release} · {platform.platform()} · "
          f"revision {rev} · {args.cpus} vCPU, {args.memory} MiB · rounds {args.rounds}, "
          f"batch {args.batch}")
    for name, kernel in kernels.items():
        first = records[name][0]
        print(f"{name}: {os.path.basename(kernel)} sha256 {digest(kernel)} · {first.get('kernel')} · "
              f"bpf_jit_enable {first.get('jit')} · filter {first.get('filter')} · "
              f"device filter {first.get('device')} insns")
        for v, said in first["vuln"].items():
            print(f"   {v}: {said}")
    print(f"\n{'arm':<18} {'kernel':<6} {'n':>8} {'p50':>9} {'p90':>9} {'p99':>9} {'max':>10}"
          f"  {'round p50s':<20} (ns per call)")
    arms = list(records["A"][0]["arms"])
    for arm in arms:
        for name in kernels:
            pooled, round_p50 = [], []
            for rec in records[name]:
                batch, values = rec["arms"][arm]
                per = sorted(v / batch for v in values)
                pooled.extend(per)
                round_p50.append(percentile(per, 50))
            pooled.sort()
            row = [percentile(pooled, 50), percentile(pooled, 90), percentile(pooled, 99), pooled[-1]]
            print(f"{arm:<18} {name:<6} {len(pooled):>8} "
                  + " ".join(f"{x:>9.1f}" for x in row[:3]) + f" {row[3]:>10.1f}"
                  + f"  {min(round_p50):.1f}-{max(round_p50):.1f}")
    print("\nMemAvailable, KiB, p50 over rounds (boot / device filter / arms / end)")
    for name in kernels:
        cols = []
        for at in ("boot", "device", "filters", "end"):
            v = sorted(int(rec["mem"][at][1]) for rec in records[name] if rec["mem"][at][1] != "?")
            cols.append(str(percentile(v, 50)) if v else "?")
        print(f"   {name}: " + " / ".join(cols))
    print("vmalloc areas of the JIT, bytes, at the end of the first round")
    for name in kernels:
        areas = records[name][0]["vmalloc"]
        print(f"   {name}: " + (", ".join(f"{c} {s}" for c, s in areas.items()) or "none"))
    if args.out:
        with open(args.out, "w") as f:
            json.dump({"args": vars(args), "revision": rev, "records": records}, f)


if __name__ == "__main__":
    sys.exit(main())
