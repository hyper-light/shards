"""M26 (docs/research/platform-measurements.md): pooled runs through two daemons, alike
but for QoS, alternating so that both arms see the same host.

Build with qos.patch applied (`git apply`), which makes SHARDS_QOS=interactive raise the
client, the daemon's threads, the warm VM's workload thread and its virtio-vsock worker
to user-interactive QoS. DIR holds that build's `shards` and `shardsd` (signed on macOS
with resources/hvf.entitlements), and homeA and homeB, each a copy of a home with IMAGE
pulled and the guest recorded (`shards guest use`). Arm B's daemon and warm VMs inherit
SHARDS_QOS from the client that starts them.

    python3 ab.py DIR IMAGE N
"""
import os, sys, time
bins, img, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
shards = os.path.join(bins, 'shards')
base = {k: v for k, v in os.environ.items() if k != 'SHARDS_QOS'}
arms = {
    'default': dict(base, SHARDS_HOME=os.path.join(bins, 'homeA')),
    'interactive': dict(base, SHARDS_HOME=os.path.join(bins, 'homeB'), SHARDS_QOS='interactive'),
}
args = [shards, 'run', '--pull', 'never', img, 'exit', '0']
fa = [(os.POSIX_SPAWN_OPEN, fd, '/dev/null', os.O_RDWR, 0) for fd in (0, 1, 2)]
def run(env):
    t = time.perf_counter()
    pid = os.posix_spawn(shards, args, env, file_actions=fa)
    _, status = os.waitpid(pid, 0)
    dt = (time.perf_counter() - t) * 1000
    assert os.waitstatus_to_exitcode(status) == 0, status
    return dt
for env in arms.values():
    for _ in range(4): run(env)
samples = {k: [] for k in arms}
for i in range(n):
    order = list(arms) if i % 2 == 0 else list(reversed(list(arms)))
    for k in order:
        samples[k].append(run(arms[k]))
        time.sleep(0.01)
def q(v, f): v = sorted(v); return v[min(len(v) - 1, int(f * len(v)))]
for k, v in samples.items():
    print(f"{k:12} n {len(v)} p50 {q(v,.5):.2f} p90 {q(v,.9):.2f} p99 {q(v,.99):.2f} max {max(v):.2f} ms")
for k, a in arms.items():
    os.spawnve(os.P_WAIT, shards, [shards, 'daemon', 'stop'], a)
