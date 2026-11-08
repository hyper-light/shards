#!/usr/bin/env python3
"""PM M497 (D44): what a build step's devices cost its start. A guest kernel booted
--rounds times on this host, each boot one round of the test guest's `stepcgroup` mode
(crates/testguest/src/linux.rs `step_cgroup_costs`), which starts --n children of each
arm, interleaved, as init starts a `RUN` step (crates/init/src/build.rs step_cgroup):

    run.py --shards SHARDS --guest TESTGUEST --kernel KERNEL
           [--rounds 5] [--n 1000] [--cpus 1] [--memory 512]

    fork       clone3 alone: a step without limits as it was started before
    steps      a cgroup of its own under `steps` (confined once by BuildKit's default
               device rules), the child cloned into it, the cgroup removed once reaped
    devices    the same under `devices`, its own program loaded and attached first
    confine    the default program's load and attach on `steps`, once a boot

SHARDS boots each (`run --kernel`; on macOS a copy carrying its signed VM process, as the
E2E tests' target/e2e/shards); TESTGUEST is the test guest built for the guest (static
musl, `--profile guest`), PID 1. Each boot's record goes to a disk of its own (/dev/vda).
Reports per arm n, p50, p90, p99 and max in µs per start, pooled over the rounds, with the
host, OS and revision. On macOS run it as a user's program, outside an agent session's QoS
clamp: docs/research/measurements/macos-sched/launchd-run.sh OUT python3 run.py ...
"""

import argparse
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile


def percentile(v, p):
    # The nearest rank, as bpf-jit/ab.py.
    rank = max(1, math.ceil(p / 100 * len(v)))
    return v[min(rank, len(v)) - 1]


def boot(args, work, env):
    disk = os.path.join(work, "record")
    with open(disk, "wb") as f:
        f.truncate(max(4 << 20, args.n * 4 * 24 // (1 << 20) * (1 << 20) + (1 << 20)))
    cmdline = f"console=ttyS0 quiet panic=-1 shards_test=stepcgroup shards_step_n={args.n}"
    r = subprocess.run(
        [
            args.shards, "run", "--kernel", args.kernel, "--init", args.guest,
            "--cpus", str(args.cpus), "--memory", str(args.memory),
            "--disk", disk, "--cmdline", cmdline,
        ],
        env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=1800,
    )
    if r.returncode != 0 or "SHARDS-TEST PASS" not in r.stdout:
        raise SystemExit(
            f"exit {r.returncode}\n--- stdout\n{r.stdout[-4000:]}\n--- stderr\n{r.stderr[-4000:]}"
        )
    arms, kernel, ended = {}, "", False
    with open(disk, "rb") as f:
        for line in f.read().rstrip(b"\0").decode().splitlines():
            word, _, rest = line.partition(" ")
            if word == "arm":
                name, _batch, *values = rest.split()
                arms[name] = [int(v) for v in values]
            elif word == "kernel":
                kernel = rest
            elif word == "end":
                ended = True
    if not ended:
        raise SystemExit("a record cut short")
    return kernel, arms


def main():
    ap = argparse.ArgumentParser()
    for flag in ("--shards", "--guest", "--kernel"):
        ap.add_argument(flag, required=True)
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--n", type=int, default=1000)
    ap.add_argument("--cpus", type=int, default=1)
    ap.add_argument("--memory", type=int, default=512)
    args = ap.parse_args()
    work = tempfile.mkdtemp(prefix="step-devices-")
    os.makedirs(os.path.join(work, "docker"))
    env = dict(os.environ, DOCKER_CONFIG=os.path.join(work, "docker"))
    pooled, kernel = {}, ""
    try:
        for _ in range(args.rounds):
            kernel, arms = boot(args, work, env)
            for name, values in arms.items():
                pooled.setdefault(name, []).extend(values)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    rev = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True,
        cwd=os.path.dirname(os.path.abspath(__file__)),
    ).stdout.strip()
    u = platform.uname()
    print(f"host {u.node} {u.machine} · {u.system} {u.release} · {platform.platform()} · "
          f"revision {rev} · guest {kernel} · {args.cpus} vCPU, {args.memory} MiB · "
          f"rounds {args.rounds} of {args.n}")
    print(f"{'arm':<9} {'n':>7} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>9}  (µs per start)")
    for name in ("fork", "steps", "devices", "confine"):
        v = sorted(x / 1000 for x in pooled.get(name, []))
        if not v:
            continue
        print(f"{name:<9} {len(v):>7} {percentile(v, 50):>8.1f} {percentile(v, 90):>8.1f} "
              f"{percentile(v, 99):>8.1f} {v[-1]:>9.1f}")


if __name__ == "__main__":
    sys.exit(main())
