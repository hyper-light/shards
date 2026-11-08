"""Templates saved one after another, for two builds alternating (PM M98): each turn stops
an arm's daemon, removes the templates in its home, and runs IMAGE once, which boots a VM
that saves the image's template and records what its run touches, the template's
working set, which the daemon takes and writes. Run with each build's daemon probed, the
followers' loop's time for each WORKING_SET part is what the arms differ in. Reported:
each run's wall clock.

    python3 working_sets.py IMAGE TURNS ARM...

Each ARM directory holds a build's `shards`, `shardsd`, `shards-vm` and `shards-net`,
signed as scripts/hvf-run signs them, and a `home` with IMAGE pulled and the guest
recorded (build-ab/ab.py's layout). ENV_<ARM's basename, upper-cased> adds `K=V` pairs to
that arm's environment, and so to its daemon's.
"""
import os, platform, shutil, subprocess, sys, time

image, turns, arms = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
walls = {arm: [] for arm in arms}


def env_of(arm):
    env = dict(os.environ, SHARDS_HOME=os.path.join(arm, "home"))
    key = "ENV_" + os.path.basename(arm.rstrip("/")).upper().replace("-", "_")
    env.update(kv.split("=", 1) for kv in os.environ.get(key, "").split())
    return env


for t in range(turns):
    for arm in (arms if t % 2 == 0 else list(reversed(arms))):
        env, shards = env_of(arm), os.path.join(arm, "shards")
        subprocess.run([shards, "daemon", "stop"], env=env, capture_output=True)
        shutil.rmtree(os.path.join(arm, "home", "templates"), ignore_errors=True)
        t0 = time.perf_counter()
        done = subprocess.run([shards, "run", "--rm", "--pull", "never", image, "true"], env=env,
                              capture_output=True)
        walls[arm].append(time.perf_counter() - t0)
        assert done.returncode == 0, done

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()} {platform.platform()}, rev {rev}, "
      f"{turns} turns, load {os.getloadavg()[0]:.2f}")
for arm in arms:
    w = sorted(walls[arm])
    q = lambda f: w[min(len(w) - 1, int(f * len(w)))]
    print(f"{arm}: n={len(w)} p50={q(.5) * 1000:.1f} ms p90={q(.9) * 1000:.1f} ms max={w[-1] * 1000:.1f} ms")
