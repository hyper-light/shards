#!/usr/bin/env python3
"""A cold boot's setup, two builds of the VM process interleaved: how long each takes from
its start to its vCPU's first entry into the guest (`entry_us` of the `shards-timing` line
it writes as it ends), booting the same kernel and test guest, one that powers off once
booted (`shards_test=work`, no work). For the kernel's load overlapped with the VM's
assembly (vm/{x86_64,aarch64}.rs `build`).

    ab.py SHARDS KERNEL INIT VM_A VM_B [BOOTS]

SHARDS is the `shards` that runs each (`run --kernel`), VM_A and VM_B the VM processes it
starts (SHARDS_VM_BINARY), each boot alternating which goes first.
"""
import json, os, statistics, subprocess, sys

shards, kernel, init, a, b = sys.argv[1:6]
boots = int(sys.argv[6]) if len(sys.argv) > 6 else 30
argv = [shards, "run", "--kernel", kernel, "--init", init, "--memory", "256", "--cmdline",
        "console=ttyS0 quiet panic=-1 shards_test=work shards_work_mib=0"]


def entry(vm):
    r = subprocess.run(argv, env=dict(os.environ, SHARDS_VM_BINARY=vm, SHARDS_TIMING="1"), capture_output=True,
                       text=True, timeout=60)
    for line in r.stderr.splitlines():
        if line.startswith("shards-timing "):
            return json.loads(line[len("shards-timing "):])["entry_us"]
    sys.exit(f"{vm}: no shards-timing line; status {r.returncode}\n{r.stderr[-2000:]}")


results = {a: [], b: []}
for i in range(boots + 2):
    order = [a, b] if i % 2 == 0 else [b, a]
    for vm in order:
        e = entry(vm)
        if i >= 2:
            results[vm].append(e)


def q(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(p * len(v)))]


for name, vm in [("A", a), ("B", b)]:
    v = results[vm]
    print(f"{name} {vm}: n {len(v)} entry_us p50 {statistics.median(v):.0f} p90 {q(v, 0.9)} p99 {q(v, 0.99)} max {max(v)}")
diffs = [x - y for x, y in zip(results[b], results[a])]
print(f"B less A, paired: p50 {statistics.median(diffs):+.0f} us")
