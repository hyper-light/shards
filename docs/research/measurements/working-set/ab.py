"""Pooled runs from one daemon and one template, whose warm VMs each take one prefetch
arm by process ID (PM M30), with experiment.patch applied:

- 0: none;
- 1: the list's pages read from vCPU 0, MMU off, before its state is restored;
- 2: as 1, after the host has copied the pages the run wrote;
- 3: the pages read, and those the run wrote given an atomic add of zero, from vCPU 0
  with its MMU on (what shards does now).

SHARDS_PREFETCH_ARM=A puts every VM in arm A. Reports each arm's time in the guest
(request to answer, the VM's clock), the client's wall clock, the prefetch's own time and
the VM's footprint, and each arm's median against arm 0 with a bootstrap 95% interval.

    SHARDS_HOME=… SHARDS_PREFETCH=LIST python3 ab.py SHARDS N [COMMAND...]

LIST comes from list.py, and the page records it reads from a run with
SHARDS_TRACK_PAGES=DIR set (pages.py summarizes them).
"""
import json, os, random, subprocess, sys, time

shards, n = sys.argv[1], int(sys.argv[2])
command = sys.argv[3:] or ["true"]
env = dict(os.environ, SHARDS_TIMING="1")
samples = {}
for i in range(n + 6):
    start = time.perf_counter()
    done = subprocess.run([shards, "run", "--rm", "--pull", "never", "alpine"] + command,
                          env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.PIPE)
    wall = (time.perf_counter() - start) * 1e6
    assert done.returncode == 0, done
    line = next(l for l in done.stderr.decode().splitlines() if l.startswith("shards-timing "))
    t = json.loads(line.split(" ", 1)[1])
    if i >= 6:
        samples.setdefault(t["prefetch"], []).append(
            (wall, t["answered_us"] - t["request_us"], t["prefetch_us"], t["rss_kib"], t["footprint_kib"], t["waiting_kib"]))
    # The pool refills between a user's runs.
    time.sleep(0.02)
subprocess.run([shards, "daemon", "stop"], env=env)


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


rng = random.Random(0)
print("load %.2f %.2f %.2f" % os.getloadavg())
for part, k in (("guest", 1), ("wall", 0), ("prefetch", 2), ("rss_kib", 3), ("footprint", 4), ("waiting", 5)):
    for arm in sorted(samples):
        v = [s[k] for s in samples[arm]]
        print(f"{part:8} arm {arm} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f}")
for part, k in (("guest", 1), ("wall", 0)):
    base = [s[k] for s in samples.get(0, [])]
    for arm in sorted(samples):
        if arm == 0 or not base:
            continue
        v = [s[k] for s in samples[arm]]
        boot = sorted(q([rng.choice(v) for _ in v], .5) - q([rng.choice(base) for _ in base], .5)
                      for _ in range(2000))
        print(f"{part:8} arm {arm} - arm 0: median {q(v, .5) - q(base, .5):+.0f} us, "
              f"95% [{boot[50]:+.0f}, {boot[1949]:+.0f}]")
