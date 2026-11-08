"""M72 (docs/research/platform-measurements.md): what a launch of a sandboxed shards-vm
costs when the previous launch under its App Sandbox identifier (`dev.shards.vm`, its
container) was another build's.

A and B are two builds' shards-vm, each ad-hoc signed with resources/vm.entitlements and
`-o runtime`, so with one identifier and different code. Each launch is `--version`,
spawn to reap, with stdio on /dev/null; the first two of each sequence are dropped.

    python3 identity-switch.py A B
"""
import os, sys, time

a, b = sys.argv[1], sys.argv[2]
fa = [(os.POSIX_SPAWN_OPEN, fd, "/dev/null", os.O_RDWR, 0) for fd in (0, 1, 2)]


def once(p):
    t = time.perf_counter()
    pid = os.posix_spawn(p, [p, "--version"], os.environ, file_actions=fa)
    os.waitpid(pid, 0)
    return (time.perf_counter() - t) * 1e3


def q(v):
    v = sorted(v)
    return f"p50 {v[len(v) // 2]:.1f} p90 {v[int(len(v) * .9)]:.1f} max {v[-1]:.1f} ms"


print("load %.2f" % os.getloadavg()[0])
for name, seq in (("A only", [a] * 60), ("B only", [b] * 60), ("A,B alternating", [a, b] * 30), ("A only again", [a] * 60)):
    v = [once(p) for p in seq][2:]
    time.sleep(0.2)
    print(f"{name:16} n {len(v)} {q(v)}")
