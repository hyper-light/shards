"""An A/B of shards-api image starts between two builds (PM M593): api_image's
`image_start_costs` run by each build's checkout in turn, ABBA, so that the host's drift
falls on both arms alike (PM M29).

Each round runs the harness once in each arm, in alternating order, `SHARDS_IMAGE_COST_N=N`
starts of each kind (Booted, Saved, Restored, Warm) in each, every process with homes, an
image and a pool of its own; each process's output is kept in OUT as ARM-ROUND.log. Then, per
arm and kind, over all its rounds' samples (`IMAGE-START-SAMPLE`): n, p50, p90, p99 and max
(nearest rank, as `common::measured` reports them) of the boot (hex.image.boot's span), the
first output and the stop; and from each round's timed phase pass (`IMAGE-START-PHASES
timed`, one start of each kind) the run host's writable layer: its launch's wait from `its
network decided` to its `its writable layer ...` step, and, where the layer was made on a
thread of its own (D253's start path cut (a)), that thread's making. With the host, its OS,
each arm's revision, and the load averages before and after.

A_DIR and B_DIR are checkouts of the two revisions, each built in its own `target` before
any start is measured. On macOS run this as the user's own (macos-sched/launchd-run.sh), so
that its processes are not clamped at utility QoS.

    python3 ab.py A_DIR B_DIR ROUNDS N OUT
"""
import os, platform, re, subprocess, sys

a_dir, b_dir, rounds, n, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]
arms = {"A": os.path.abspath(a_dir), "B": os.path.abspath(b_dir)}
test = ["cargo", "test", "-p", "shards", "--profile", "release", "--test", "api_image"]
# The rustup proxy on PATH, with no DYLD_*, or rust-lld cannot load libLLVM (CLAUDE.md).
base = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}


def env(arm, **more):
    return dict(base, CARGO_TARGET_DIR=os.path.join(arms[arm], "target"), **more)


os.makedirs(out)
for arm in arms:
    subprocess.run(test + ["--no-run"], cwd=arms[arm], env=env(arm), check=True)
revision = {
    arm: subprocess.run(
        ["git", "rev-parse", "--short=12", "HEAD"], cwd=d, capture_output=True, text=True, check=True
    ).stdout.strip()
    for arm, d in arms.items()
}
load = [os.getloadavg()]
logs = {arm: [] for arm in arms}
for r in range(rounds):
    for arm in ["A", "B"] if r % 2 == 0 else ["B", "A"]:
        log = os.path.join(out, f"{arm}-{r}.log")
        with open(log, "w") as f:
            done = subprocess.run(
                test + ["--", "--ignored", "--exact", "image_start_costs", "--nocapture"],
                cwd=arms[arm],
                env=env(arm, SHARDS_IMAGE_COST_N=n),
                stdout=f,
                stderr=subprocess.STDOUT,
            )
        if done.returncode != 0:
            sys.exit(f"{arm}'s round {r} failed: {log}")
        logs[arm].append(log)
load.append(os.getloadavg())

SAMPLE = re.compile(r"IMAGE-START-SAMPLE round \d+ (\w+) from \w+ boot_us (\d+) usable_us (\d+) stop_us (\d+)")
PHASE = re.compile(r"IMAGE-START-PHASES timed (\w+) +(-?\d+) +(?:\+-?\d+)? +(\S.*)$")
THREAD = re.compile(r"^run host +its writable layer \w+ on its own thread, in (\d+) us")
TAKEN = re.compile(r"^run host +its writable layer (made|cloned|none)")
SPANS = ("boot", "first output", "stop")


def stats(v):
    """n, p50, p90, p99 and max of `v` (microseconds), in ms."""
    if not v:
        return "n 0"
    v = sorted(v)

    def at(p):
        return v[max((len(v) * p + 99) // 100, 1) - 1] / 1000

    return f"n {len(v)} p50 {at(50):.3f} p90 {at(90):.3f} p99 {at(99):.3f} max {v[-1] / 1000:.3f} ms"


print(
    f"host {platform.node()}, {platform.system()} {platform.release()} {platform.machine()}, "
    f"{os.cpu_count()} CPUs; load {load[0]} before, {load[1]} after; {rounds} rounds ABBA, N {n}"
)
for arm, files in logs.items():
    samples, waits, made = {}, {}, {}
    for log in files:
        decided = {}
        with open(log, errors="replace") as f:
            for line in f:
                m = SAMPLE.search(line)
                if m:
                    for i, us in enumerate(m.groups()[1:]):
                        samples.setdefault(m[1], ([], [], []))[i].append(int(us))
                    continue
                m = PHASE.search(line)
                if not m:
                    continue
                kind, at, text = m[1], int(m[2]), m[3]
                t = THREAD.search(text)
                if t:
                    made.setdefault(kind, []).append(int(t[1]))
                elif text.startswith("run host") and text.endswith("its network decided"):
                    decided[kind] = at
                elif TAKEN.search(text) and kind in decided:
                    waits.setdefault(kind, []).append(at - decided.pop(kind))
    print(f"{arm} {revision[arm]} ({arms[arm]}), logs {', '.join(files)}")
    for kind, spans in samples.items():
        for span, v in zip(SPANS, spans):
            print(f"  {kind:9} {span:20} {stats(v)}")
        print(f"  {kind:9} {'layer: launch waited':20} {stats(waits.get(kind, []))}")
        print(f"  {kind:9} {'layer: its thread':20} {stats(made.get(kind, []))}")
