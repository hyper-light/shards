#!/usr/bin/env python3
"""Two guest kernels, A and B, booted and restored on this host in turn.

    ab.py --bin DIR --init FILE --guest FILE --a KERNEL --b KERNEL [--runs N]
          [--a-cmdline ARGS] [--b-cmdline ARGS]

--bin is a directory holding shards, shardsd and shards-vm (on macOS, signed copies such
as the E2E tests' target/e2e/shards-*). --init is shards-init, which boots; --guest is
the test guest (crates/testguest), whose `resume` mode takes the snapshot restored.
--a-cmdline and --b-cmdline add to each arm's kernel command line, so one kernel can be
set against itself booted another way (`preempt=full`).

Each sample is a fresh shards process. Pairs run in the order A B, then B A, and so on,
so drift on the host falls on both kernels alike. After 3 warm-ups of each, it reports
n, p50, p90, p99 and max, in microseconds from the VMM's own clock, of:

- boot_kernel: the boot vCPU enters the guest -> PID 1 starts (INIT_STARTED)
- boot_to_init: VMM main -> PID 1 starts
- restore: VMM main -> the guest runs again (RESUMED), restoring a snapshot taken
  with that kernel
"""

import argparse
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile

INIT_STARTED = 1
RESUMED = 2
WARMUP = 3


def timing(stderr):
    for line in stderr.splitlines():
        if line.startswith("shards-timing "):
            return json.loads(line[len("shards-timing "):])
    raise SystemExit(f"no shards-timing line in:\n{stderr}")


def marker(t, which):
    for m, us in t.get("markers", []):
        if m == which:
            return us
    return None


def shards(bin_dir, *args):
    env = dict(os.environ, SHARDS_TIMING="1")
    r = subprocess.run(
        [os.path.join(bin_dir, "shards"), *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if r.returncode != 0:
        raise SystemExit(f"shards {' '.join(args)}: exit {r.returncode}\n{r.stderr}")
    return timing(r.stderr)


def boot(bin_dir, kernel, init, extra):
    t = shards(
        bin_dir, "vm", "run", "--kernel", kernel, "--init", init, "--cpus", "1",
        "--memory", "256", "--cmdline", f"quiet panic=-1 {extra}".strip(), "--no-console",
    )
    init_us = marker(t, INIT_STARTED)
    return {"boot_kernel": init_us - t["entry_us"], "boot_to_init": init_us}


def snapshot(bin_dir, kernel, guest, dir, extra):
    shards(
        bin_dir, "vm", "run", "--kernel", kernel, "--init", guest, "--cpus", "1",
        "--memory", "256", "--cmdline", f"quiet panic=-1 shards_test=resume {extra}".strip(),
        "--no-console", "--snapshot-dir", dir,
    )


def restore(bin_dir, dir):
    return {"restore": marker(shards(bin_dir, "vm", "restore", dir, "--no-console"), RESUMED)}


def percentile(v, p):
    # As crates/shards/benches/support: the nearest rank.
    rank = max(1, math.ceil(p / 100 * len(v)))
    return v[min(rank, len(v)) - 1]


def main():
    ap = argparse.ArgumentParser()
    for flag in ("--bin", "--init", "--guest", "--a", "--b"):
        ap.add_argument(flag, required=True)
    ap.add_argument("--runs", type=int, default=100)
    ap.add_argument("--a-cmdline", default="")
    ap.add_argument("--b-cmdline", default="")
    args = ap.parse_args()
    kernels = {"A": args.a, "B": args.b}
    extras = {"A": args.a_cmdline, "B": args.b_cmdline}
    work = tempfile.mkdtemp(prefix="kernel-ab-")
    try:
        snaps = {}
        for name, kernel in kernels.items():
            snaps[name] = os.path.join(work, name)
            snapshot(args.bin, kernel, args.guest, snaps[name], extras[name])
        samples = {name: {} for name in kernels}

        def one(name, keep):
            got = boot(args.bin, kernels[name], args.init, extras[name])
            got.update(restore(args.bin, snaps[name]))
            if keep:
                for metric, us in got.items():
                    samples[name].setdefault(metric, []).append(us)

        for i in range(WARMUP + args.runs):
            for name in ("A", "B") if i % 2 == 0 else ("B", "A"):
                one(name, i >= WARMUP)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    rev = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True
    ).stdout.strip()
    print(f"host {platform.machine()} {platform.platform()} · revision {rev} · n={args.runs}")
    for name, kernel in kernels.items():
        print(f"{name}: {os.path.basename(kernel)} {extras[name]}".rstrip())
    print(f"{'metric':<14} {'kernel':<6} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>8}  (us)")
    for metric in samples["A"]:
        for name in kernels:
            v = sorted(samples[name][metric])
            row = [percentile(v, 50), percentile(v, 90), percentile(v, 99), v[-1]]
            print(f"{metric:<14} {name:<6} " + " ".join(f"{x:>8}" for x in row))
    json.dump(samples, sys.stderr)


if __name__ == "__main__":
    main()
