"""M140 (docs/research/platform-measurements.md): where a `RUN --mount=type=cache` keeps its
files, and what keeping them across builds costs (D114).

- Step: fsbench's phases in one builder step, interleaved, on a cache mount (an overlay of
  the record's layers under the builder's memory) and on a tmpfs mount (the builder's own
  memory, where caches lived before D114); the same phases on a virtio-fs share (`shards
  run -v`, the device a share of host files would use) and that run's tmpfs.
- Warm: a cache filled by an earlier build (10,000 files of 4 KiB and 2 of 128 MiB), read
  back from the record's layers, against the same files in a tmpfs mount of the step.
- Import and export: a build's wall time to mount that cache and do nothing, against an
  empty cache; and to fill a cache, against filling a tmpfs mount.

    python3 -I cachefs.py SHARDS WORK BUILDS REPS SHARE_REPS

SHARDS is a `shards` with D114, WORK an empty directory with a short path (the builder's
sockets live under it). Each variant is built BUILDS times, alternating, each build's step
new to the build cache by an `ENV` of its own: `--no-cache` would let go of the caches it
mounts (D114).
"""
import os, platform, re, statistics, subprocess, sys, time

shards, work, builds, reps, share_reps = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
here = os.path.dirname(os.path.abspath(__file__))
arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}[platform.machine()]
ctx = os.path.join(work, "ctx")
os.makedirs(ctx, exist_ok=True)
subprocess.run(["rustc", "--edition", "2024", "-O", "--target", f"{arch}-unknown-linux-musl",
                "-C", "linker=rust-lld", "-o", os.path.join(ctx, "fsbench"),
                os.path.join(here, "fsbench.rs")], check=True)
env = dict(os.environ, SHARDS_HOME=os.path.join(work, "home"))
os.makedirs(env["SHARDS_HOME"], exist_ok=True)
bind = "--mount=type=bind,source=fsbench,target=/fsbench"
built = 0


def build(run_line, tag=None):
    global built
    built += 1
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        copy = "COPY fsbench /fsbench\n" if tag else ""
        f.write(f"FROM scratch\n{copy}ENV CACHEFS_BUILD={built}\n{run_line}\n")
    args = [shards, "build", "--progress=plain"] + (["-t", tag] if tag else ["-o", os.path.join(work, "out")]) + [ctx]
    t = time.monotonic()
    p = subprocess.run(args, env=env, capture_output=True, text=True, cwd=work)
    took = time.monotonic() - t
    if p.returncode != 0:
        sys.exit(f"build failed:\n{p.stderr}")
    return took, p.stderr


def warm(log):
    # A cache a build left, read back: fsbench says when it had to fill one instead.
    if "/warm\tfilled\t" in log:
        sys.exit(f"the warm cache was empty:\n{log}")
    return log


def phases(log, into):
    for line in log.splitlines():
        m = re.search(r"(/\w+)\t(\w+)\t(\d+)$", line)
        if m and m.group(2) != "filled":
            into.setdefault((m.group(1), m.group(2)), []).append(int(m.group(3)) / 1e6)


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


def show(title, results, unit="ms"):
    print(f"== {title}")
    for k, v in sorted(results.items()):
        print(f"{k[0]:>8} {k[1]:>10}  n {len(v):>3}  p50 {q(v,.5):9.1f}  p90 {q(v,.9):9.1f}  p99 {q(v,.99):9.1f}  max {max(v):9.1f} {unit}")


# Step: a cache mount against the step's tmpfs.
step = {}
for _ in range(builds):
    _, log = build(f'RUN {bind} --mount=type=cache,target=/cache,id=step --mount=type=tmpfs,target=/tmpfs ["/fsbench", "{reps}", "/cache", "/tmpfs"]')
    phases(log, step)
show("step: cache mount and tmpfs, ms", step)

# The same on a virtio-fs share, against the run's own tmpfs.
share = os.path.join(work, "share")
os.makedirs(share, exist_ok=True)
build('RUN ["/fsbench", "0"]', tag="cachefs-bench:1")
p = subprocess.run([shards, "run", "--rm", "-m", "2g", "-v", f"{share}:/share", "--tmpfs", "/tmpfs:size=1g",
                    "cachefs-bench:1", "/fsbench", str(share_reps), "/share", "/tmpfs"],
                   env=env, capture_output=True, text=True, cwd=work)
if p.returncode != 0:
    sys.exit(f"run failed:\n{p.stderr}")
shared = {}
phases(p.stdout, shared)
show("run: virtio-fs share and tmpfs, ms", shared)

# Warm: a cache an earlier build filled, read back, against a tmpfs the step fills.
build(f'RUN {bind} --mount=type=cache,target=/warm,id=warm ["/fsbench", "fill", "/warm"]')
reread = {}
for _ in range(builds):
    _, log = build(f'RUN {bind} --mount=type=cache,target=/warm,id=warm --mount=type=tmpfs,target=/tmpfs ["/fsbench", "reread", "{reps}", "/warm", "/tmpfs"]')
    phases(warm(log), reread)
show("warm: a filled cache read back, and tmpfs, ms", reread)

# Import and export, by the build's wall time.
walls = {}
for i in range(builds):
    for name, line in [
        ("empty", f'RUN {bind} --mount=type=cache,target=/e,id=empty{i} ["/fsbench", "0"]'),
        ("filled", f'RUN {bind} --mount=type=cache,target=/warm,id=warm ["/fsbench", "reread", "0", "/warm"]'),
        ("tmpfs-fill", f'RUN {bind} --mount=type=tmpfs,target=/x ["/fsbench", "fill", "/x"]'),
        ("cache-fill", f'RUN {bind} --mount=type=cache,target=/x,id=x{i} ["/fsbench", "fill", "/x"]'),
    ]:
        took, log = build(line)
        warm(log)
        walls.setdefault(("build", name), []).append(took * 1e3)
show("import and export: a build's wall time, ms", walls)
print("load %.2f %.2f %.2f" % os.getloadavg())
