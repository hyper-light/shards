"""P3.1's warm start at Shards' level (kiln plan 01a1dd2e313f72f69c6d93061143cc2e, step P3.1:
"warm agent start, end to end (p50, n>=20) on KVM and HVF is at most 300 ms, measured by
committed benchmark reporting n, p50, p90, p99, max"): an image's run started through
shards-api's `ImagePools`, the call Hex makes once P3.1 lands, against the same run started
through `ImageShards`, the call Hex makes now (hex-runtime guest/image.rs `host_images`).

api_image's `image_start_costs` makes both starts in one process by turns, ABBA
(`SHARDS_IMAGE_COST_KINDS=Restored,Warm`), each timed from its `boot_stoppable` call to its
member's first output read, and held to the kind asked. This runs that process with
image-start-ab's runner, on the host as it is (`quiet`) and with each CPU this process may run
on kept busy (`loaded`), the two by turns, until each condition's p99s are stable. It reports
each condition's n, p50, p90, p99 and max (image-start-ab's statistics), each p99's
distribution-free 95% interval, the warm start's paired difference from the cold one, and
P3.1's verdict, with the host, its OS and the revision; OUT keeps every process's log and the
report (summary.txt). README.md beside this file says how to run it and where each number
comes from.

    python3 warm.py DIR OUT [--n N] [--budget SECONDS]
"""
import argparse, enum, importlib.util, math, os, platform, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "image_start_ab", os.path.join(HERE, "..", "image-start-ab", "ab.py")
)
ab = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ab)

# P3.1's bar: a warm agent start's p50, end to end, at most 300 ms over at least 20 starts (kiln
# plan 01a1dd2e313f72f69c6d93061143cc2e, step P3.1's quantitative acceptance criterion).
BAR_MS = 300
BAR_N = 20
# The quantile whose stability ends sampling, and the confidence of its interval, as
# s33-injector-ab's: the p99 is the tail the person's rule holds first-class, and 95% the
# conventional two-sided level of a distribution-free quantile interval (Le Boudec,
# Performance Evaluation of Computer and Communication Systems, 2010, §2.4.3 and Theorem 2.1).
QUANTILE = 0.99
CONFIDENCE = 0.95
# The arms, as image_start_costs names its kinds: `ImageShards`, each start restoring its
# image's template (D25), and `ImagePools`, each start taking a warm host (D253).
COLD, WARM = "Restored", "Warm"
KINDS = (COLD, WARM)
USABLE = ab.SPANS.index("first output")
CONDITIONS = ("quiet", "loaded")


def interval(n, q):
    """The 1-based order statistics (j, k) whose [X(j), X(k)] holds the `q` quantile of `n`
    samples at CONFIDENCE, each tail within half the rest (Le Boudec 2010, Theorem 2.1): j the
    highest with B(j - 1) within it, k the lowest with B(k - 1) at least 1 less it, B being
    Binomial(n, q)'s CDF; None where n is too few for one. s33-injector-ab's `interval`."""
    tail = (1 - CONFIDENCE) / 2
    lq, lp = math.log(q), math.log(1 - q)
    # cdf[i] = P(Binomial(n, q) <= i), each term taken in logs so that none overflows.
    ln_choose, total, cdf = 0.0, 0.0, []
    for i in range(n + 1):
        if i:
            ln_choose += math.log(n - i + 1) - math.log(i)
        total += math.exp(ln_choose + i * lq + (n - i) * lp)
        cdf.append(total)
    lows = [j for j in range(1, n + 1) if cdf[j - 1] <= tail]
    highs = [k for k in range(1, n + 1) if cdf[k - 1] >= 1 - tail]
    return (max(lows), min(highs)) if lows and highs else None


def least_n():
    """The fewest samples whose p99 has an interval at all: 368 at 95%."""
    n = 1
    while interval(n, QUANTILE) is None:
        n += 1
    return n


def bounded(v, p):
    """Sorted `v`'s nearest-rank `p`th percentile, and the two of `v` its 95% interval spans
    (None where `v` is too few for one)."""
    ci = interval(len(v), p / 100)
    return ab.nearest(v, p), ci and (v[ci[0] - 1], v[ci[1] - 1])


def stable(now, previous):
    """Whether each series' p99 (`now`: its p99 and interval) lies within the interval its round
    before gave it (`previous`), as s33-injector-ab's `sample` stops."""
    return previous is not None and all(lo <= p99 <= hi for (p99, _), (lo, hi) in zip(now, previous))


def met(v):
    """P3.1's verdict on sorted warm first-output times `v` (µs): whether their p50 is at most
    BAR_MS; None under BAR_N starts, which its bar asks at least."""
    return None if len(v) < BAR_N else ab.nearest(v, 50) <= BAR_MS * 1000


def samples(logs):
    """Each kind's spans over `logs`, in µs (image-start-ab's SAMPLE lines: boot, first output,
    stop), and each round's warm first output less its cold one: a round is one start of each
    kind, their lines one after the other in their process's log."""
    spans = {kind: tuple([] for _ in ab.SPANS) for kind in KINDS}
    paired = []
    for log in logs:
        said = []
        with open(log, errors="replace") as f:
            for line in f:
                m = ab.SAMPLE.search(line)
                if not m:
                    continue
                if m[1] not in KINDS:
                    sys.exit(
                        f"{log}: a {m[1]} start, never asked: "
                        "its image_start_costs predates SHARDS_IMAGE_COST_KINDS"
                    )
                said.append((m[1], [int(us) for us in m.groups()[1:]]))
        for kind, us in said:
            for v, t in zip(spans[kind], us):
                v.append(t)
        for r in range(0, len(said) - len(KINDS) + 1, len(KINDS)):
            usable = {kind: us[USABLE] for kind, us in said[r : r + len(KINDS)]}
            if len(usable) == len(KINDS):
                paired.append(usable[WARM] - usable[COLD])
    return spans, paired


def cpus():
    """The CPUs this process may run on (its affinity, where the OS keeps one)."""
    if hasattr(os, "sched_getaffinity"):
        return len(os.sched_getaffinity(0))
    n = os.process_cpu_count() if hasattr(os, "process_cpu_count") else os.cpu_count()
    if not n:
        sys.exit("this OS says no count of its CPUs, which the loaded condition keeps busy")
    return n


class Busy:
    """Where `on`, one process spinning in user space on each CPU this process may run on, from
    entry to exit: s33-injector-ab's `S33_AB_LOAD=cpus`, as processes, as the starts measured
    run in processes of their own."""

    def __init__(self, on):
        self.n = cpus() if on else 0
        self.procs = []

    def __enter__(self):
        for _ in range(self.n):
            spin = [sys.executable, "-I", "-c", "while True: pass"]
            self.procs.append(subprocess.Popen(spin, stdin=subprocess.DEVNULL))
        return self.n

    def __exit__(self, *_):
        for p in self.procs:
            p.kill()
        for p in self.procs:
            p.wait()


class Stop(enum.Enum):
    """Why a condition's sampling ended."""

    STABLE = "each p99 within its interval of the round before"
    FIXED = "at --n"
    BUDGET = "--budget spent, its p99s still moving"
    SKIPPED = "its test said SKIP"


class Condition:
    """One way the host is held while starts are measured, its processes' logs, the starts of
    each kind it wants by its round's end, and why its sampling ended."""

    def __init__(self, name, target):
        self.name, self.target = name, target
        self.logs, self.loads = [], []
        self.previous, self.stop, self.rounds = None, None, 0


def batch(c, d, out, r):
    """`c`'s process of round `r`, in checkout `d`: the starts its target is past its samples,
    each CPU kept busy where `c` is `loaded`."""
    have = len(samples(c.logs)[0][WARM][USABLE])
    log = os.path.join(out, f"{c.name}-{r}.log")
    env = dict(
        ab.base, SHARDS_IMAGE_COST_N=str(c.target - have), SHARDS_IMAGE_COST_KINDS=",".join(KINDS)
    )
    with Busy(c.name == "loaded") as busy:
        before = ab.loadavg()
        ab.measure(d, env, log, f"{c.name}'s round {r}")
        c.loads.append((busy, before, ab.loadavg()))
    c.logs.append(log)


def settle(c, a, began):
    """After `c`'s round: whether its sampling ends, and where not, its next target, doubled."""
    c.rounds += 1
    spans, _ = samples(c.logs)
    if not spans[WARM][USABLE]:
        c.stop = Stop.SKIPPED
        return
    if a.n:
        c.stop = Stop.FIXED
        return
    now = [bounded(sorted(spans[kind][USABLE]), 99) for kind in KINDS]
    if stable(now, c.previous):
        c.stop = Stop.STABLE
    elif a.budget is not None and time.monotonic() - began > a.budget:
        c.stop = Stop.BUDGET
    else:
        c.previous = [ci for _, ci in now]
        c.target *= 2


def ms(us):
    return f"{us / 1000:.3f}"


def within(v, p):
    ci = bounded(v, p)[1] if v else None
    return f"[{ms(ci[0])}, {ms(ci[1])}]" if ci else "none (too few)"


def skipped(logs):
    for log in logs:
        with open(log, errors="replace") as f:
            for line in f:
                if "SKIP:" in line:
                    return line.strip()
    return "no start measured, and no SKIP said"


def report(d, conds, a):
    lines = []

    def say(line):
        print(line)
        lines.append(line)

    dirty = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=d,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    mac = f" (macOS {platform.mac_ver()[0]})" if platform.system() == "Darwin" else ""
    say("p3-warm-start: P3.1's warm start, shards-api's ImagePools against ImageShards (README.md)")
    say(
        f"host {platform.node()}, {platform.system()} {platform.release()} {platform.machine()}{mac}, "
        f"{os.cpu_count()} CPUs; revision {ab.revision(d)}{' with uncommitted changes' if dirty else ''} ({d})"
    )
    say(
        f"n: {a.n} starts of each kind a condition (--n)"
        if a.n
        else f"n: from {least_n()} starts of each kind a condition, doubled until each p99 is stable"
    )
    say("times in ms from shards-api's boot_stoppable call; 'first output': the member's, read by the caller")
    for c in conds:
        logs = ", ".join(c.logs)
        if c.stop is Stop.SKIPPED:
            say(f"{c.name}: unverified, {c.stop.value}: {skipped(c.logs)}; logs {logs}")
            continue
        loads = "; ".join(f"{n} busy, load {x} before, {y} after" for n, x, y in c.loads)
        say(f"{c.name}: stopped {c.stop.value}, after {c.rounds} rounds; {loads}; logs {logs}")
        spans, paired = samples(c.logs)
        for kind, arm in ((COLD, "cold: ImageShards"), (WARM, "warm: ImagePools")):
            for span, v in zip(ab.SPANS, spans[kind]):
                v = sorted(v)
                say(f"  {arm:17} {span:12} {ab.stats(v)}, p99 95% {within(v, 99)}")
        paired.sort()
        if paired:
            say(
                f"  warm - cold, paired by round, first output: median {ms(ab.nearest(paired, 50))} ms, "
                f"95% {within(paired, 50)}, n {len(paired)}"
            )
        warm = sorted(spans[WARM][USABLE])
        verdict = met(warm)
        if verdict is None:
            say(f"  P3.1: no verdict: n {len(warm)}, under the {BAR_N} its bar asks")
            continue
        p50 = ab.nearest(warm, 50)
        say(
            f"  P3.1 (a warm start to its first output, p50 at most {BAR_MS} ms over n >= {BAR_N}): "
            f"{'met' if verdict else 'MISSED'}: p50 {ms(p50)} ms, 95% {within(warm, 50)}, n {len(warm)}; "
            f"left of it for Hex's own steps {ms(BAR_MS * 1000 - p50)} ms"
        )
    with open(os.path.join(a.out, "summary.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")


def main():
    p = argparse.ArgumentParser(description="P3.1's warm start at Shards' level (README.md)")
    p.add_argument("dir", help="the checkout measured, built here in its own target")
    p.add_argument("out", help="a new directory for every process's log and summary.txt")
    p.add_argument("--n", type=int, help="starts of each kind a condition, in place of the stability rule")
    p.add_argument("--budget", type=float, help="seconds after which no condition starts another round")
    a = p.parse_args()
    d = os.path.abspath(a.dir)
    os.makedirs(a.out)
    subprocess.run(ab.test + ["--no-run"], cwd=d, env=ab.base, check=True)
    began = time.monotonic()
    conds = [Condition(name, a.n or least_n()) for name in CONDITIONS]
    r = 0
    while going := [c for c in conds if c.stop is None]:
        for c in ab.order(r, going):
            batch(c, d, a.out, r)
        for c in going:
            settle(c, a, began)
        r += 1
    report(d, conds, a)


if __name__ == "__main__":
    main()
