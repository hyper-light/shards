"""M75 (docs/research/platform-measurements.md): K launches of a binary at once, signed
into App Sandbox (resources/vm.entitlements, `-o runtime`) and not (resources/
hvf.entitlements), each `--version`, spawn to reap: whether sandboxed launches share a
bottleneck.

    python3 concurrent.py SANDBOXED_BINARY PLAIN_BINARY
"""
import os, sys, time, threading
fa = [(os.POSIX_SPAWN_OPEN, fd, "/dev/null", os.O_RDWR, 0) for fd in (0, 1, 2)]
def launch(p, out):
    t = time.perf_counter(); pid = os.posix_spawn(p, [p, "--version"], os.environ, file_actions=fa); os.waitpid(pid, 0); out.append((time.perf_counter() - t) * 1e3)
for name, p in (("App Sandbox", sys.argv[1]), ("no sandbox", sys.argv[2])):
    for k in (1, 2, 4, 8, 16, 32):
        res = []
        for rep in range(max(1, 64 // k)):
            out = []; ths = [threading.Thread(target=launch, args=(p, out)) for _ in range(k)]
            [t.start() for t in ths]; [t.join() for t in ths]; res += out
        res.sort(); q = lambda f: res[min(len(res)-1, int(f*len(res)))]
        print(f"{name:12} {k:2} at once: n {len(res)} p50 {q(.5):6.1f} p90 {q(.9):6.1f} max {res[-1]:6.1f} ms")
print("load %.2f" % os.getloadavg()[0])
