"""A daemon's first run, through two builds, alternating so that both arms see the same
host (docs/research/platform-measurements.md M167): what a run costs that finds a daemon
just started, which compiles its seccomp filter for the first time, as the first run after
each daemon start does.

Each DIR holds a build's `shards` and a `home` with IMAGE pulled, whose first run has saved
its template. Each turn stops an arm's daemon (`shards stop daemon`), then times one
`shards run --rm --pull never IMAGE COMMAND...` through it, which starts a daemon of its
own: the client's wall clock, spawn to reap, split by the timing line SHARDS_TIMING adds
into the command's time in the guest (request to status, the VM's clock) and the rest.
The arms alternate which goes first; the median of their paired differences is reported
with a bootstrap 95% interval.

On macOS the two builds' VM processes share one App Sandbox identity, and a launch after
the other build's pays about 100 ms (M72): each run here launches its VM on its path,
after the other arm's, so both pay it alike.

    python3 first-run.py OLD_DIR NEW_DIR IMAGE N [COMMAND...]
"""
import json, os, random, subprocess, sys, time

old, new, image, n = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
command = sys.argv[5:] or ["true"]
arms = {}
for name, d in (("old", old), ("new", new)):
    env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"), SHARDS_TIMING="1")
    arms[name] = (os.path.join(d, "shards"), env)


def stop(shards, env):
    subprocess.run([shards, "stop", "daemon"], env=env, stdin=subprocess.DEVNULL,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def first_run(shards, env):
    stop(shards, env)
    start = time.perf_counter()
    done = subprocess.run(
        [shards, "run", "--rm", "--pull", "never", image] + command,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    wall = (time.perf_counter() - start) * 1e6
    assert done.returncode == 0, done
    line = next(l for l in done.stderr.decode().splitlines() if l.startswith("shards-timing "))
    timing = json.loads(line.split(" ", 1)[1])
    return wall, timing["answered_us"] - timing["request_us"]


# Each home's template saved, and each build's helpers written out, before any is timed.
for shards, env in arms.values():
    first_run(shards, env)
samples = {name: [] for name in arms}
for i in range(n):
    order = list(arms) if i % 2 == 0 else list(reversed(list(arms)))
    for name in order:
        samples[name].append(first_run(*arms[name]))


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


rng = random.Random(0)
print("load %.2f %.2f %.2f" % os.getloadavg())
for part, of in (("wall", lambda w, c: w), ("command", lambda w, c: c), ("outside", lambda w, c: w - c)):
    a = [of(*s) for s in samples["old"]]
    b = [of(*s) for s in samples["new"]]
    for name, v in (("old", a), ("new", b)):
        print(f"{part:8} {name} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f} us")
    d = [y - x for x, y in zip(a, b)]
    boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
    print(f"{part:8} new - old, paired: median {q(d, .5):+.0f} us, 95% [{boot[50]:+.0f}, {boot[1949]:+.0f}]")
for shards, env in arms.values():
    stop(shards, env)
