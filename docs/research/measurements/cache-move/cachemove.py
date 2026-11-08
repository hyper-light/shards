"""M194 (docs/research/platform-measurements.md): where moving a cache mount in and out of
the builder spends a build's time (D114's open item, AGENTFILE_ARCH §11 row 979), and what
D134 changed of it.

    python3 -I cachemove.py WORK BUILDS SHARDS [SHARDS...]

Each SHARDS is a release `shards` built with `--features alloc-count`, whose builds mark each
part of a step's cache mounts, each line `alloc-phase NAME ms=…`:

- before D134 (build/exec.rs at 22cf16b7): `cache-take` the records taken and their layers
  made a tree on the host, `cache-send` that tree sent to the builder, `run` the step,
  `cache-changes` the step's changes to a cache read back and applied, `layer` and `blob`
  their layer written and stored, `cache-keep` the record kept; and, as `alloc-split` lines,
  of `cache-changes` the part spent applying them (`applying`), the rest waiting on the
  builder, and of `cache-send` the part spent waiting on the builder to take its frames
  (`framing`), the rest this process reading and framing the tree;
- from D134: `cache-take` the records taken, `cache-attach` their layers and a file for
  each one's changes attached to the builder's join disk, `run` the step, `cache-keep` the
  changes kept; and of `cache-keep`, the part spent waiting on the builder to write their
  image (`writing`) and keeping it, its layers merged as the record says (`storing`).

WORK is an empty directory with a short path (the builders' sockets live under it). Each
SHARDS has a home of its own there, and the turns take them in turn, each turn starting
with the next, so that each meets the host's load as the others do.

Each turn builds, with each SHARDS in turn:

- `empty`: a step that mounts an empty cache and does nothing;
- `filled`: a step that mounts the cache a first build filled (M140's: 10,000 files of 4
  KiB in 100 directories, and 2 of 128 MiB) and does nothing;
- `fill`: a step that fills a cache of its own, new each turn.

Each build's step is new to the build cache by an `ENV` of its own (`--no-cache` would let
go of the caches it mounts, D114). Reports each part's milliseconds and each build's wall
clock: n, p50, p90, p99, max, for each SHARDS by its position; and each later SHARDS's wall
clock less the first's, turn by turn, as the median of the differences with a bootstrap 95%
interval; then each build's wall clock as it came (`raw`).
"""
import json, os, platform, random, re, statistics, subprocess, sys, time

work, builds, binaries = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
here = os.path.dirname(os.path.abspath(__file__))
arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}[platform.machine()]
ctx = os.path.join(work, "ctx")
os.makedirs(ctx, exist_ok=True)
subprocess.run(["rustc", "--edition", "2024", "-O", "--target", f"{arch}-unknown-linux-musl",
                "-C", "linker=rust-lld", "-o", os.path.join(ctx, "fsbench"),
                os.path.join(here, "..", "cachefs", "fsbench.rs")], check=True)
bind = "--mount=type=bind,source=fsbench,target=/fsbench"
built = 0
PARTS = ["cache-take", "cache-send", "framing", "cache-attach", "run", "cache-changes", "applying",
         "layer", "blob", "cache-keep", "writing", "storing"]


def env_of(i):
    home = os.path.join(work, f"home{i}")
    os.makedirs(home, exist_ok=True)
    return dict(os.environ, SHARDS_HOME=home)


def build(shards, env, run_line):
    global built
    built += 1
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write(f"FROM scratch\nENV CACHEMOVE_BUILD={built}\n{run_line}\n")
    t = time.monotonic()
    p = subprocess.run([shards, "build", "--progress=plain", "-o", os.path.join(work, "out"), ctx],
                       env=env, capture_output=True, text=True, cwd=work)
    took = (time.monotonic() - t) * 1e3
    if p.returncode != 0:
        sys.exit(f"build failed:\n{p.stderr}")
    if "/warm\tfilled\t" in p.stdout + p.stderr:
        sys.exit(f"the filled cache was empty:\n{p.stderr}")
    return took, p.stderr


def parts(log):
    """Each part of the step's cache mounts: the phases from `cache-take` to `cache-keep`,
    the step's own commit after it left out."""
    out, on = {}, False
    for line in log.splitlines():
        s = re.match(r"alloc-split \S+ (applying|framing|writing|storing) ms=([\d.]+)", line)
        if s:
            if on or s.group(1) in ("writing", "storing"):
                out[s.group(1)] = out.get(s.group(1), 0.0) + float(s.group(2))
            continue
        m = re.match(r"alloc-phase (\S+) ms=([\d.]+)", line)
        if not m:
            continue
        name, ms = m.group(1), float(m.group(2))
        if name == "cache-take":
            on = True
        if on and name in PARTS:
            out[name] = out.get(name, 0.0) + ms
        if name == "cache-keep":
            on = False
    return out


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


envs = [env_of(i) for i in range(len(binaries))]
for shards, env in zip(binaries, envs):
    build(shards, env, f'RUN {bind} --mount=type=cache,target=/warm,id=warm ["/fsbench", "fill", "/warm"]')
results = {}
for i in range(builds):
    order = list(range(len(binaries)))
    order = order[i % len(order):] + order[:i % len(order)]
    for b in order:
        shards, env = binaries[b], envs[b]
        for name, line in [
            ("empty", f'RUN {bind} --mount=type=cache,target=/e,id=empty{i} ["/fsbench", "0"]'),
            ("filled", f'RUN {bind} --mount=type=cache,target=/warm,id=warm ["/fsbench", "reread", "0", "/warm"]'),
            ("fill", f'RUN {bind} --mount=type=cache,target=/x,id=x{i} ["/fsbench", "fill", "/x"]'),
        ]:
            took, log = build(shards, env, line)
            for part, ms in parts(log).items():
                results.setdefault((b, name, part), []).append(ms)
            results.setdefault((b, name, "wall"), []).append(took)
    print(f"turn {i + 1}/{builds}", file=sys.stderr)
for b, shards in enumerate(binaries):
    print(f"#### {b}: {shards}")
    for name in ["empty", "filled", "fill"]:
        print(f"== {name}")
        for part in PARTS + ["wall"]:
            v = results.get((b, name, part))
            if v:
                print(f"  {part:>14}  n {len(v):>3}  p50 {q(v,.5):9.1f}  p90 {q(v,.9):9.1f}  p99 {q(v,.99):9.1f}  max {max(v):9.1f} ms")
# Each later SHARDS against the first, turn by turn (each turn built both under one load):
# the median of the differences, with a bootstrap 95% interval.
rng = random.Random(0)
for b in range(1, len(binaries)):
    for name in ["empty", "filled", "fill"]:
        d = [y - x for x, y in zip(results[(0, name, "wall")], results[(b, name, "wall")])]
        boot = sorted(statistics.median(rng.choices(d, k=len(d))) for _ in range(10000))
        print(f"{b} less 0, {name}: median {statistics.median(d):.1f} ms [{boot[250]:.1f}, {boot[9749]:.1f}]")
print("raw " + json.dumps({f"{b} {n} {p}": v for (b, n, p), v in results.items() if p == "wall"}))
print("load %.2f %.2f %.2f" % os.getloadavg())
