#!/usr/bin/env python3
"""shards build against docker build on generated contexts, interleaved.

Each context is FROM alpine:3.22, COPY . /app, over FILES files of BYTES bytes each in
directories of 100, made from a fixed seed. Each round runs both builders once, which
goes first alternating; docker runs with --no-cache, as shards has no cache yet, and -q.
Wall time is spawn to exit. Reports n, p50, p90, p99 and max per builder.

  bench.py SHARDS_BIN WORKDIR FILES BYTES RUNS
"""
import os, random, shutil, statistics, subprocess, sys, time

shards_bin, work, files, size, runs = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
ctx = os.path.join(work, f"ctx-{files}-{size}")
home = os.path.join(work, "home")
if not os.path.isdir(ctx):
    rnd = random.Random(1)
    os.makedirs(ctx)
    for i in range(files):
        d = os.path.join(ctx, f"d{i // 100}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}"), "wb") as f:
            f.write(rnd.randbytes(size))
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write("FROM alpine:3.22\nCOPY . /app\n")
os.makedirs(home, exist_ok=True)
os.chmod(home, 0o700)
env = dict(os.environ, SHARDS_HOME=home)
cmds = {
    "shards": [shards_bin, "build", "-q", ctx],
    "docker": ["docker", "build", "--no-cache", "-q", ctx],
}
def once(cmd):
    t = time.perf_counter()
    r = subprocess.run(cmd, env=env, capture_output=True)
    dt = time.perf_counter() - t
    if r.returncode != 0:
        sys.exit(f"{cmd[0]} failed: {r.stderr.decode()[-2000:]}")
    return dt
for name in cmds:  # warm-up: base pulled, caches warm
    once(cmds[name])
times = {k: [] for k in cmds}
for i in range(runs):
    order = list(cmds) if i % 2 == 0 else list(reversed(cmds))
    for k in order:
        times[k].append(once(cmds[k]))
def pct(xs, p):
    xs = sorted(xs)
    return xs[max(0, -(-len(xs) * p // 100) - 1)]
print(f"files={files} bytes={size} total={files*size/1e6:.1f}MB n={runs}")
print("| builder | p50 | p90 | p99 | max |\n|---|---|---|---|---|")
for k, xs in times.items():
    print(f"| {k} | {pct(xs,50)*1000:.0f} ms | {pct(xs,90)*1000:.0f} ms | {pct(xs,99)*1000:.0f} ms | {max(xs)*1000:.0f} ms |")
