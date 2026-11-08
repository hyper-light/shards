"""`shards logs` of a container whose log holds RECORDS records of one short line each, as
a command writing a line at a time leaves it, one in ten on stderr (review 7.10): the
command's wall clock and the daemon's CPU time, for builds alternating.

Each ARM directory holds a build's `shards`, `shardsd` and `shards-vm`, and a `home` with
IMAGE pulled and the guest recorded (build-ab/ab.py's layout). A container is run in each
home, and its log and index replaced by those of RECORDS records, as its writer writes
them (spec.rs: a 13-byte head, stream, time and length, then the output; an index entry
of 8 bytes for each, its start, stream and whether it ends a line). Then `shards logs` is
asked for RUNS times in each arm, the arms alternating, its output drained and counted.

    python3 read.py IMAGE RECORDS RUNS ARM...
"""
import os, platform, subprocess, sys, time

image, records, runs, arms = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4:]
INDEX_STDERR, INDEX_LINE = 1 << 63, 1 << 62


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    total = 0.0
    for part in out.split(":"):
        total = total * 60 + float(part)
    return total


expected = 0
setup = {}
for arm in arms:
    env = dict(os.environ, SHARDS_HOME=os.path.join(arm, "home"))
    shards = os.path.join(arm, "shards")
    subprocess.run([shards, "rm", "-f", "chatty"], env=env, capture_output=True)
    run = subprocess.run([shards, "run", "--name", "chatty", "--pull", "never", image, "true"],
                         env=env, capture_output=True)
    assert run.returncode == 0, run
    listed = subprocess.run([shards, "ps", "-a", "--no-trunc"], env=env, capture_output=True,
                            text=True).stdout
    cid = next(l.split()[0] for l in listed.splitlines() if l.endswith(" chatty"))
    d = os.path.join(arm, "home", "containers", cid)
    log, index = bytearray(), bytearray()
    expected = 0
    for i in range(records):
        out = b"line %09d\n" % i
        stream = 2 if i % 10 == 9 else 1
        entry = len(log) | INDEX_LINE | (INDEX_STDERR if stream == 2 else 0)
        index += entry.to_bytes(8, "big")
        log += bytes([stream]) + (1_700_000_000_000_000_000 + i).to_bytes(8, "big") + len(out).to_bytes(4, "big") + out
        expected += len(out)
    with open(os.path.join(d, "log"), "wb") as f:
        f.write(log)
    with open(os.path.join(d, "log.idx"), "wb") as f:
        f.write(index)
    pid = int(open(os.path.join(arm, "home", "daemon.pid")).read())
    setup[arm] = (shards, env, pid)

walls = {arm: [] for arm in arms}
cpus = {arm: [] for arm in arms}
for r in range(runs):
    for arm in (arms if r % 2 == 0 else list(reversed(arms))):
        shards, env, pid = setup[arm]
        before = cpu_seconds(pid)
        t0 = time.perf_counter()
        p = subprocess.Popen([shards, "logs", "chatty"], env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE)
        got = 0
        out, err = p.communicate()
        got = len(out) + len(err)
        walls[arm].append(time.perf_counter() - t0)
        cpus[arm].append(cpu_seconds(pid) - before)
        assert p.returncode == 0 and got == expected, (arm, p.returncode, got, expected)

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()} {platform.platform()}, rev {rev}, "
      f"{records} records, {expected} bytes, load {os.getloadavg()[0]:.2f}")
for arm in arms:
    w = sorted(walls[arm])
    q = lambda f: w[min(len(w) - 1, int(f * len(w)))]
    print(f"{arm}: logs n={len(w)} p50={q(.5) * 1000:.1f} ms p90={q(.9) * 1000:.1f} ms "
          f"max={w[-1] * 1000:.1f} ms; daemon cpu p50 {sorted(cpus[arm])[len(w) // 2]:.2f} s")
for arm in arms:
    shards, env, _ = setup[arm]
    subprocess.run([shards, "rm", "-f", "chatty"], env=env, capture_output=True)
