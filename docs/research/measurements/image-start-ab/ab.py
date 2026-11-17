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

Its runner and statistics are p3-warm-start/warm.py's too, which imports them.

    python3 ab.py A_DIR B_DIR ROUNDS N OUT
"""
import os, platform, re, subprocess, sys

test = ["cargo", "test", "-p", "shards", "--profile", "release", "--test", "api_image"]
costs = test + ["--", "--ignored", "--exact", "image_start_costs", "--nocapture"]
# The rustup proxy on PATH, with no DYLD_*, or rust-lld cannot load libLLVM (CLAUDE.md).
base = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}

SAMPLE = re.compile(r"IMAGE-START-SAMPLE round \d+ (\w+) from \w+ boot_us (\d+) usable_us (\d+) stop_us (\d+)")
PHASE = re.compile(r"IMAGE-START-PHASES timed (\w+) +(-?\d+) +(?:\+-?\d+)? +(\S.*)$")
THREAD = re.compile(r"^run host +its writable layer \w+ on its own thread, in (\d+) us")
TAKEN = re.compile(r"^run host +its writable layer (made|cloned|none)")
SPANS = ("boot", "first output", "stop")


def revision(d):
    return subprocess.run(
        ["git", "rev-parse", "--short=12", "HEAD"], cwd=d, capture_output=True, text=True, check=True
    ).stdout.strip()


def loadavg():
    """The 1, 5 and 15 minute load averages, where the OS keeps them (Windows keeps none)."""
    return os.getloadavg() if hasattr(os, "getloadavg") else "none kept by this OS"


def order(r, names):
    """Round `r`'s turns: `names` in order, then reversed, by turns (ABBA)."""
    return list(names) if r % 2 == 0 else list(reversed(list(names)))


def measure(cwd, env, log, who):
    """One `image_start_costs` process in checkout `cwd`, its output kept in `log`; where it
    fails, this harness ends, naming `who` and `log`."""
    with open(log, "w") as f:
        done = subprocess.run(costs, cwd=cwd, env=env, stdout=f, stderr=subprocess.STDOUT)
    if done.returncode != 0:
        sys.exit(f"{who} failed: {log}")


def nearest(v, p):
    """Sorted `v`'s nearest-rank `p`th percentile: the least of them that p% of them are
    within, as `common::measured` takes it."""
    return v[max((len(v) * p + 99) // 100, 1) - 1]


def stats(v):
    """n, p50, p90, p99 and max of `v` (microseconds), in ms."""
    if not v:
        return "n 0"
    v = sorted(v)
    return (
        f"n {len(v)} p50 {nearest(v, 50) / 1000:.3f} p90 {nearest(v, 90) / 1000:.3f} "
        f"p99 {nearest(v, 99) / 1000:.3f} max {v[-1] / 1000:.3f} ms"
    )


def main(a_dir, b_dir, rounds, n, out):
    arms = {"A": os.path.abspath(a_dir), "B": os.path.abspath(b_dir)}

    def env(arm, **more):
        return dict(base, CARGO_TARGET_DIR=os.path.join(arms[arm], "target"), **more)

    os.makedirs(out)
    for arm in arms:
        subprocess.run(test + ["--no-run"], cwd=arms[arm], env=env(arm), check=True)
    revisions = {arm: revision(d) for arm, d in arms.items()}
    load = [loadavg()]
    logs = {arm: [] for arm in arms}
    for r in range(rounds):
        for arm in order(r, arms):
            log = os.path.join(out, f"{arm}-{r}.log")
            measure(arms[arm], env(arm, SHARDS_IMAGE_COST_N=n), log, f"{arm}'s round {r}")
            logs[arm].append(log)
    load.append(loadavg())

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
        print(f"{arm} {revisions[arm]} ({arms[arm]}), logs {', '.join(files)}")
        for kind, spans in samples.items():
            for span, v in zip(SPANS, spans):
                print(f"  {kind:9} {span:20} {stats(v)}")
            print(f"  {kind:9} {'layer: launch waited':20} {stats(waits.get(kind, []))}")
            print(f"  {kind:9} {'layer: its thread':20} {stats(made.get(kind, []))}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5])
