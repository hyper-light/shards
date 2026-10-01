"""Who answers a warm VM's grants on macOS (D30): the daemon, on the VM's watch thread, or
a `shardsd grants` process spawned for each VM, as `shards vm` has. Two daemons of one
build, alike but for SHARDS_GRANT_BROKER, alternating so that both arms see the same host.

Build with broker.patch applied (`git apply`). It makes SHARDS_GRANT_BROKER=spawn have
the daemon spawn the broker in place of answering, and logs, for each warm VM that comes
up, `grant-broker ready-us N`: microseconds from the start of its spawn (the broker's
first, in the spawn arm) to its READY, every grant answered. Each VM also writes to the
daemon's log how long its first ask waited (`wait-us`) and, per grant, when its answer
came (`answer-us ACCESS N`), which show where a difference is
(docs/research/platform-measurements.md M71). DIR holds that build's
`shards` and `shardsd` (signed with resources/hvf.entitlements) and `shards-vm` (signed
with resources/vm.entitlements and `-o runtime`), and homeA and homeB, each a copy of a
home with IMAGE pulled, the guest recorded and the template saved. The daemons inherit
SHARDS_GRANT_BROKER from the client that starts them.

Each run uses a warm VM, and its daemon starts another: those starts are the samples, and
each run's wall clock (client spawn to reap) shows whether either arm reaches the run's
path. AB_PAUSE (seconds, default 0.3) follows each run, long enough for the refill.

    python3 ab.py DIR IMAGE N
"""
import os, random, subprocess, sys, time

bins, image, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
shards = os.path.join(bins, "shards")
base = {k: v for k, v in os.environ.items() if k != "SHARDS_GRANT_BROKER"}
arms = {
    "daemon": dict(base, SHARDS_HOME=os.path.join(bins, "homeA")),
    "spawned": dict(base, SHARDS_HOME=os.path.join(bins, "homeB"), SHARDS_GRANT_BROKER="spawn"),
}
args = [shards, "run", "--pull", "never", image, "true"]
fa = [(os.POSIX_SPAWN_OPEN, fd, "/dev/null", os.O_RDWR, 0) for fd in (0, 1, 2)]
pause = float(os.environ.get("AB_PAUSE", "0.3"))


def run(env):
    t = time.perf_counter()
    pid = os.posix_spawn(shards, args, env, file_actions=fa)
    _, status = os.waitpid(pid, 0)
    dt = (time.perf_counter() - t) * 1e6
    assert os.waitstatus_to_exitcode(status) == 0, status
    return dt


def readies(env):
    with open(os.path.join(env["SHARDS_HOME"], "daemon.log")) as f:
        return [int(l.split()[-1]) for l in f if "grant-broker ready-us" in l]


for env in arms.values():
    subprocess.run([shards, "daemon", "stop"], env=env, stderr=subprocess.DEVNULL)
    for _ in range(8):
        run(env)
        time.sleep(pause)
skip = {name: len(readies(env)) for name, env in arms.items()}
wall = {name: [] for name in arms}
for i in range(n):
    order = list(arms) if i % 2 == 0 else list(reversed(list(arms)))
    for name in order:
        wall[name].append(run(arms[name]))
        time.sleep(pause)
ready = {name: readies(env)[skip[name]:] for name, env in arms.items()}
for env in arms.values():
    subprocess.run([shards, "daemon", "stop"], env=env)


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


rng = random.Random(0)
print("load %.2f %.2f %.2f" % os.getloadavg())
for part, samples in (("ready", ready), ("wall", wall)):
    for name, v in samples.items():
        print(f"{part:5} {name:7} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f} us")
    a, b = samples["daemon"], samples["spawned"]
    d = [y - x for x, y in zip(a, b)]
    boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
    print(f"{part:5} spawned - daemon, paired: median {q(d, .5):+.0f} us, 95% [{boot[50]:+.0f}, {boot[1949]:+.0f}]")
