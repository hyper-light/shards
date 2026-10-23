"""Builds through two releases of `shards`, interleaved so that both arms see the same
host: what a builder that boots small, its memory plugged as its build needs it, costs a
build and saves the host, against one booted with all its memory (PM M193, D133).

    python3 ab.py OLD_DIR NEW_DIR HOME N

Each DIR holds a release `shards` and its `shards-vm`, the latter signed (on macOS) with
an identifier of its own: the two arms' VM processes would otherwise share one App
Sandbox identity, and each launch after the other arm's would pay for the switch (PM M72).
Each arm's builds start its own (SHARDS_VM_BINARY). HOME is a SHARDS_HOME with
alpine:3.22 pulled, which both arms build from: a build takes its base from the store, and
writes nothing there but the image it makes, which no build reads back.

Each turn builds both arms back to back, in random order, each Dockerfile, with
`--no-cache` so that every RUN runs:

- `steps`: M82's three RUNs over alpine:3.22 (files, a user, a step as that user in a
  WORKDIR), which need little memory;
- `grow`: one RUN that writes 4 GiB to a file and removes it, which a builder booted with
  512 MiB has plugged for it as it goes.

A sample is the build's wall clock, spawn to exit, and the builder's peak resident set, from
the line SHARDS_TIMING has its VM process print as it ends (its footprint there is of a VM
already gone). The new arm's line costs it 2.5 ms, its look at the 63.5 GiB to plug, counted
in its wall. Two turns first warm both arms up, the first launch of each VM process being
assessed by macOS (PM M166), and are not counted. Reports n, p50, p90, p99 and max per arm
and Dockerfile, and the median of the paired differences, new less old, with a bootstrap 95%
interval.
"""
import atexit, json, os, random, shutil, statistics, subprocess, sys, tempfile, time

old_dir, new_dir, home, n = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
DOCKERFILES = {
    "steps": "FROM alpine:3.22\n"
    "RUN mkdir -p /opt/app && echo hello > /opt/app/f && chmod 640 /opt/app/f\n"
    "RUN adduser -D app\n"
    "USER app\n"
    "WORKDIR /home/app/work\n"
    "RUN id && pwd && ls -l /opt/app\n",
    "grow": "FROM alpine:3.22\nRUN dd if=/dev/zero of=/big bs=1M count=4096 && rm /big\n",
}
scratch = tempfile.mkdtemp(prefix="builder-memory-")
atexit.register(shutil.rmtree, scratch, ignore_errors=True)
contexts = {}
for name, dockerfile in DOCKERFILES.items():
    ctx = os.path.join(scratch, name)
    os.makedirs(ctx)
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write(dockerfile)
    contexts[name] = ctx


def build(arm_dir, ctx):
    env = dict(
        os.environ,
        SHARDS_HOME=home,
        SHARDS_TIMING="1",
        SHARDS_VM_BINARY=os.path.join(arm_dir, "shards-vm"),
    )
    start = time.perf_counter()
    done = subprocess.run(
        [os.path.join(arm_dir, "shards"), "build", "--no-cache", "--progress=plain", ctx],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    wall = time.perf_counter() - start
    err = done.stderr.decode(errors="replace")
    if done.returncode != 0:
        sys.exit(f"{arm_dir} {ctx}: exit {done.returncode}\n{err}")
    timing = [json.loads(l.split("shards-timing ", 1)[1]) for l in err.splitlines() if "shards-timing " in l]
    if not timing:
        sys.exit(f"{arm_dir} {ctx}: no timing line\n{err}")
    t = timing[-1]
    return {"wall_ms": wall * 1e3, "rss_mib": t["rss_kib"] / 1024}


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


samples = {(arm, name): [] for arm in ("old", "new") for name in DOCKERFILES}
WARMUP = 2
for turn in range(-WARMUP, n):
    for name, ctx in contexts.items():
        arms = [("old", old_dir), ("new", new_dir)]
        random.shuffle(arms)
        for arm, d in arms:
            sample = build(d, ctx)
            if turn >= 0:
                samples[(arm, name)].append(sample)
    print(f"turn {turn + 1}/{n}", file=sys.stderr)

for name in DOCKERFILES:
    print(f"\n{name}")
    for metric in ("wall_ms", "rss_mib"):
        for arm in ("old", "new"):
            xs = [s[metric] for s in samples[(arm, name)]]
            print(
                f"  {metric:13s} {arm}: n={len(xs)} p50={pct(xs, .5):.1f} p90={pct(xs, .9):.1f} "
                f"p99={pct(xs, .99):.1f} max={max(xs):.1f}"
            )
        diffs = [b[metric] - a[metric] for a, b in zip(samples[("old", name)], samples[("new", name)])]
        boots = sorted(
            statistics.median(random.choices(diffs, k=len(diffs))) for _ in range(2000)
        )
        print(
            f"  {metric:13s} new-old median {statistics.median(diffs):+.1f} "
            f"[{boots[49]:+.1f}, {boots[1949]:+.1f}]"
        )
