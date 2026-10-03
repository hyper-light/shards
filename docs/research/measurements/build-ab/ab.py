"""Pooled runs through two builds' daemons, alternating so that both arms see the same
host: whether a change costs a run anything, apart from what the host is doing
(docs/research/platform-measurements.md M29).

Each DIR holds a build's `shards` and `shardsd` (the latter signed on macOS with
resources/hvf.entitlements) and a `home` with IMAGE pulled and the guest recorded
(`shards guest use`), whose first run has saved the template. Restores cost more from
some templates than others, so copy one arm's template over the other's before
comparing: both then restore the same snapshot.

AB_DUMP names a file for every turn's samples, as JSON.

ENV_OLD and ENV_NEW add `K=V` pairs to one arm's environment, and so to the daemon its
first run starts: one build can be compared with itself under a setting.

Samples are the client's wall clock, spawn to reap, of `shards run --pull never IMAGE
COMMAND...`, split by the timing line SHARDS_TIMING adds into the command's time in the
guest (request to status, the VM's clock) and the rest. Each iteration runs both arms
back to back, in alternating order, so their difference cancels what the host was doing
then: the median of those differences is reported with a bootstrap 95% interval.

AB_PAUSE (seconds, default 0.3) is the pause after each run, in which its daemon refills
its pool: long enough, and no arm's refill overlaps the next run.

AB_BURST (default 1) runs that many at once in each arm's turn, as a burst of clients
does: more than its pool keeps ready, and the rest start their VMs as they claim them.
Each run's times are reported, and paired per turn, the burst's median and its slowest.
Those launches are on the runs' path: on macOS, sign one arm's `shards-vm` with an
identifier of its own (`codesign --identifier`), or each pays the switch below.

On macOS the two builds' VM processes share one App Sandbox identity, and a launch after
the other build's pays about 100 ms (M72): each refill does, off the run's path, so a
pause shorter than that puts it on the next run's; the default leaves room for it. Compare launches themselves with one
build (grant-broker/ab.py).

    python3 ab.py OLD_DIR NEW_DIR IMAGE N [COMMAND...]
"""
import concurrent.futures, json, os, random, statistics, subprocess, sys, time

old, new, image, n = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
command = sys.argv[5:] or ["true"]
arms = {}
for name, d in (("old", old), ("new", new)):
    env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"), SHARDS_TIMING="1")
    env.update(kv.split("=", 1) for kv in os.environ.get("ENV_" + name.upper(), "").split())
    arms[name] = (os.path.join(d, "shards"), env)


def run(shards, env):
    start = time.perf_counter()
    done = subprocess.run(
        [shards, "run", "--pull", "never", image] + command,
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


burst = int(os.environ.get("AB_BURST", "1"))


def turn(shards, env):
    """One arm's turn: `burst` runs at once, each one's (wall, command)."""
    if burst == 1:
        return [run(shards, env)]
    with concurrent.futures.ThreadPoolExecutor(burst) as pool:
        return list(pool.map(lambda _: run(shards, env), range(burst)))


for shards, env in arms.values():
    for _ in range(4):
        turn(shards, env)
        time.sleep(float(os.environ.get("AB_PAUSE", "0.3")))
turns = {name: [] for name in arms}
for i in range(n):
    order = list(arms) if i % 2 == 0 else list(reversed(list(arms)))
    for name in order:
        turns[name].append(turn(*arms[name]))
        # The pool refills between a user's runs.
        time.sleep(float(os.environ.get("AB_PAUSE", "0.3")))
samples = {name: [s for t in ts for s in t] for name, ts in turns.items()}
# Each turn's runs, (wall, command) in microseconds, for a closer look.
if os.environ.get("AB_DUMP"):
    with open(os.environ["AB_DUMP"], "w") as f:
        json.dump(turns, f)


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
    if burst == 1:
        d = [y - x for x, y in zip(a, b)]
        boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
        print(f"{part:8} new - old, paired: median {q(d, .5):+.0f} us, 95% [{boot[50]:+.0f}, {boot[1949]:+.0f}]")
        continue
    for stat, f in (("median", statistics.median), ("slowest", max)):
        per = {name: [f([of(*s) for s in t]) for t in ts] for name, ts in turns.items()}
        d = [y - x for x, y in zip(per["old"], per["new"])]
        boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
        print(f"{part:8} new - old, paired per burst's {stat}: median {q(d, .5):+.0f} us, "
              f"95% [{boot[50]:+.0f}, {boot[1949]:+.0f}]")
for shards, env in arms.values():
    subprocess.run([shards, "daemon", "stop"], env=env)
