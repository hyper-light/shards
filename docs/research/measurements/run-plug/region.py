"""D148: what a region of memory to plug costs a microVM's cold boot and its end, by its
size: `shards run --kernel ... --memory 256 [--memory-max N] --rootfs ROOT -- true`,
the sizes in turn, N rounds, the client's wall clock, n, p50, p90 and max.

    python3 region.py SHARDS KERNEL INIT ROOTFS N
"""
import subprocess, sys, time

shards, kernel, init, root, n = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
sizes = [0, 2048, 16384, 65536, 131072]
arms = {s: [] for s in sizes}
for i in range(n):
    for s in sizes if i % 2 == 0 else list(reversed(sizes)):
        extra = ["--memory-max", str(s)] if s else []
        start = time.perf_counter()
        done = subprocess.run(
            [shards, "run", "--kernel", kernel, "--init", init, "--memory", "256", *extra, "--rootfs", root, "--", "true"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        assert done.returncode == 0, done.stderr[-400:]
        arms[s].append((time.perf_counter() - start) * 1e6)
for s, v in arms.items():
    v.sort()
    q = lambda p: v[min(len(v) - 1, len(v) * p // 100)]
    print(f"memory-max {s:6} MiB n={len(v)} p50={q(50):.0f} p90={q(90):.0f} max={v[-1]:.0f} us")
