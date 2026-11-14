#!/usr/bin/env python3
"""What a guest's console costs: the 16550 UART (`console=ttyS0`) against virtio-console
(`console=hvc0`). Boots the test guest's `console` mode (crates/testguest, linux.rs
`console_cost`), which writes KIB KiB to its console as 64-byte lines and waits for the
console to have sent them (tcdrain), each way N times, the ways interleaved, and each boot
again with 0 KiB, so that what the KiB cost is told apart from the boot's own.

Per boot: the guest's time to write and drain the KiB (its own clock), the host CPU time of
the VM process (getrusage of children), whether every line arrived whole, and with --perf on
Linux, KVM's exits (`perf stat -e kvm:kvm_exit`, which needs perf_event_paranoid <= 1 or
CAP_PERFMON). Reports n, p50, p90, p99 and max of: guest throughput (MiB/s), host CPU per
KiB, and exits per KiB, each the KIB boot's less its paired 0-KiB boot's.

Each way is a console, optionally with the shards build that has it (`ttyS0=/old/shards`):
the UART and virtio-console are never in one build, so the comparison runs the build before
the switch against the build after it, both on this host, interleaved.

Usage: cost.py SHARDS KERNEL TESTGUEST [--n 20] [--kib 1024] [--ways ttyS0[=SHARDS],hvc0[=SHARDS]] [--perf]
"""

import argparse
import os
import re
import resource
import shutil
import subprocess
import sys
import tempfile
import time

a = argparse.ArgumentParser()
a.add_argument("shards")
a.add_argument("kernel")
a.add_argument("testguest")
a.add_argument("--n", type=int, default=20)
a.add_argument("--kib", type=int, default=1024)
a.add_argument("--ways", default="ttyS0,hvc0")
a.add_argument("--perf", action="store_true")
args = a.parse_args()
ways = dict((w.split("=", 1) + [args.shards])[:2] for w in args.ways.split(","))
if args.perf and not shutil.which("perf"):
    sys.exit("--perf: no perf on PATH")
# No docker config of this host's is read (the tests' rule for manual runs).
env = dict(os.environ, DOCKER_CONFIG=tempfile.mkdtemp(prefix="console-cost-"))
LINE = re.compile(rb"^[!-~]{63}\r?$")


def boot(way, kib):
    cmdline = f"console={way} quiet panic=-1 shards_test=console shards_console_kib={kib}"
    cmd = [ways[way], "run", "--kernel", args.kernel, "--init", args.testguest,
           "--cpus", "1", "--memory", "256", "--cmdline", cmdline]
    stat = None
    if args.perf:
        stat = tempfile.NamedTemporaryFile(prefix="console-cost-perf-", delete=False).name
        cmd = ["perf", "stat", "-x", ",", "-e", "kvm:kvm_exit", "-o", stat, "--"] + cmd
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.monotonic()
    p = subprocess.run(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, env=env)
    wall = time.monotonic() - t0
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    said = re.search(rb"SHARDS-CONSOLE kib=(\d+) ns=(\d+)", p.stdout)
    if not said or int(said.group(1)) != kib:
        sys.exit(f"{way} {kib} KiB: no report; exit {p.returncode}\n"
                 f"{p.stdout[-2000:].decode(errors='replace')}\n"
                 f"{p.stderr[-2000:].decode(errors='replace')}")
    whole = sum(1 for line in p.stdout.split(b"\n") if LINE.match(line))
    exits = None
    if stat:
        with open(stat) as f:
            for line in f:
                fields = line.strip().split(",")
                if len(fields) > 2 and fields[2] == "kvm:kvm_exit":
                    exits = int(fields[0])
        os.unlink(stat)
    return {"guest_ns": int(said.group(2)), "cpu": cpu, "wall": wall,
            "lost": kib * 16 - whole, "exits": exits}


def summary(values, unit, fmt="{:.2f}"):
    v = sorted(values)
    if not v:
        return "unmeasured"
    pick = lambda q: v[min(len(v) - 1, int(q * len(v)))]
    return (f"n={len(v)} p50={fmt.format(pick(0.5))} p90={fmt.format(pick(0.9))} "
            f"p99={fmt.format(pick(0.99))} max={fmt.format(v[-1])} {unit}")


rows = {w: [] for w in ways}
for i in range(args.n):
    for way in ways:
        base = boot(way, 0)
        full = boot(way, args.kib)
        rows[way].append((base, full))
        print(f"round {i} {way}: guest {full['guest_ns'] / 1e6:.2f} ms, cpu "
              f"{(full['cpu'] - base['cpu']) * 1e3:.1f} ms, exits "
              f"{full['exits'] if full['exits'] is None else full['exits'] - base['exits']}, "
              f"lost lines {full['lost']}", flush=True)

host = f"{os.uname().sysname} {os.uname().release} {os.uname().machine}"
rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True,
                     text=True).stdout.strip()
print(f"console-cost {host}, revision {rev}, {args.kib} KiB, n={args.n} each way, interleaved")
for way in ways:
    r = rows[way]
    kib = args.kib
    mibs = [kib / 1024 / (f["guest_ns"] / 1e9) for _, f in r]
    cpu = [(f["cpu"] - b["cpu"]) * 1e6 / kib for b, f in r]
    exits = [(f["exits"] - b["exits"]) / kib for b, f in r if f["exits"] is not None]
    lost = sum(f["lost"] for _, f in r)
    print(f"{way}: guest throughput {summary(mibs, 'MiB/s')}")
    print(f"{way}: host CPU {summary(cpu, 'us/KiB')}")
    print(f"{way}: exits {summary(exits, 'per KiB')}")
    print(f"{way}: lines lost {lost} of {kib * 16 * len(r)}")
