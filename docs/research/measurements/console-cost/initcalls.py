#!/usr/bin/env python3
"""How early a guest's console can be virtio-console's: boots the test guest N times with
`initcall_debug` and reports, on the guest's clock, when the initcalls that bound it return:
`crash_save_vmcoreinfo_init` (subsys_initcall: the VMCOREINFO page a host can read the
printk ring through from then on), `virtio_mmio_init` and `virtio_console_init` (both
module_init: hvc0 registers, and replays the printk ring, at the later of the two, as the
driver probes its device; drivers/tty/hvc/hvc_console.c `hvc_check_console`), and
`pvpanic_mmio_driver_init` where the kernel has it. Every initcall's line goes to the console
here (not quiet), which slows the boot: the times are an upper bound.

Usage: initcalls.py SHARDS KERNEL TESTGUEST [N] [CONSOLE]

CONSOLE is ttyS0 for a build with the UART, hvc0 (the default) for one with virtio-console:
what hvc0 says before it registers it replays from the printk ring, with its times.
"""

import os
import re
import subprocess
import sys
import tempfile

shards, kernel, guest = sys.argv[1:4]
n = int(sys.argv[4]) if len(sys.argv) > 4 else 20
console = sys.argv[5] if len(sys.argv) > 5 else "hvc0"
names = ["crash_save_vmcoreinfo_init", "virtio_mmio_init", "virtio_console_init",
         "pvpanic_mmio_driver_init"]
env = dict(os.environ, DOCKER_CONFIG=tempfile.mkdtemp(prefix="console-initcalls-"))
seen = {name: [] for name in names}
for _ in range(n):
    p = subprocess.run(
        [shards, "run", "--kernel", kernel, "--init", guest, "--cpus", "1", "--memory", "256",
         "--cmdline", f"console={console} panic=-1 initcall_debug shards_test=console shards_console_kib=0"],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    out = p.stdout.decode(errors="replace")
    for name in names:
        m = re.search(r"^\[\s*(\d+\.\d+)\] initcall " + name + r"\+\S+ returned", out, re.M)
        if m:
            seen[name].append(float(m.group(1)) * 1e3)


def summary(v):
    v = sorted(v)
    if not v:
        return "not in this kernel"
    pick = lambda q: v[min(len(v) - 1, int(q * len(v)))]
    return f"n={len(v)} p50={pick(0.5):.2f} p90={pick(0.9):.2f} p99={pick(0.99):.2f} max={v[-1]:.2f} ms"


print(f"console-initcalls {os.uname().sysname} {os.uname().machine}, {n} boots")
for name in names:
    print(f"{name} returned at {summary(seen[name])}")
