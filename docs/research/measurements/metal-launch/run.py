"""PM M214: launches of `plain`, `metal`, `metal device`, `moltenvk` and `moltenvk device`, 300
each, alternating, spawn to
reap; then each held a second for its resident size (ps) and its physical footprint
(footprint). Build first: cargo build --release --manifest-path Cargo.toml.

    python3 run.py [N]
"""
import os, statistics, subprocess, sys, time

here = os.path.dirname(os.path.abspath(__file__))
d = os.path.join(here, "target", "release")
n = int(sys.argv[1]) if len(sys.argv) > 1 else 300
cases = {
    "plain": [f"{d}/plain"],
    "metal linked": [f"{d}/metal"],
    "metal device": [f"{d}/metal", "device"],
    "moltenvk linked": [f"{d}/moltenvk"],
    "moltenvk device": [f"{d}/moltenvk", "device"],
}
for argv in cases.values():
    subprocess.run(argv, check=True)
times = {k: [] for k in cases}
for _ in range(n):
    for k, argv in cases.items():
        t = time.perf_counter()
        subprocess.run(argv, check=True)
        times[k].append((time.perf_counter() - t) * 1e3)
for k, v in times.items():
    v.sort()
    q = lambda p: v[min(len(v) - 1, len(v) * p // 100)]
    print(f"{k:16} n={len(v)} p50 {statistics.median(v):6.2f} p90 {q(90):6.2f} p99 {q(99):6.2f} max {v[-1]:7.2f} ms")
held = [
    ("plain", [f"{d}/plain", "hold"]),
    ("metal linked", [f"{d}/metal", "hold"]),
    ("metal device", [f"{d}/metal", "device-hold"]),
    ("moltenvk linked", [f"{d}/moltenvk", "hold"]),
    ("moltenvk device", [f"{d}/moltenvk", "device-hold"]),
]
for k, argv in held:
    p = subprocess.Popen(argv)
    time.sleep(1.0)
    rss = subprocess.run(["ps", "-o", "rss=", "-p", str(p.pid)], capture_output=True, text=True).stdout.strip()
    fp = subprocess.run(["footprint", str(p.pid)], capture_output=True, text=True).stdout
    line = next((l for l in fp.splitlines() if "Footprint" in l), "").strip()
    print(f"{k:16} resident {rss} KiB; {line}")
    p.wait()
