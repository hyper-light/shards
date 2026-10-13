"""PM M224: whether a pass of free page reporting (D151) ends a run that grows as it comes.
Each turn runs `exit 0` through DIR's `shards` and `home` (whose pool then restores a new
microVM), pauses a time drawn uniformly from LO to HI seconds (1.0 and 2.6 by default),
then runs `alloc MIB`, and prints that run's microVM's age as its request came
(`request_us`, from its process's start), its exit status and what its guest reported
free (`reported_kib`), from its timing line (SHARDS_TIMING). The home has the guest
recorded (`shards guest use`) and IMAGE, the test guest alone (`FROM scratch`, `COPY
shards-testguest /bin/testguest`, `ENTRYPOINT ["/bin/testguest"]`); the pauses are
seeded, so arms see the same ones.

    python3 report-race.py SHARDS HOME IMAGE N MIB
"""
import json, os, random, subprocess, sys, time

shards, home, image, n, mib = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
lo, hi = float(os.environ.get("LO", "1.0")), float(os.environ.get("HI", "2.6"))
env = dict(os.environ, SHARDS_HOME=home, SHARDS_TIMING="1")
rng = random.Random(1)
for i in range(n):
    subprocess.run([shards, "run", "--rm", "--pull", "never", image, "exit", "0"], env=env,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(rng.uniform(lo, hi))
    done = subprocess.run([shards, "run", "--rm", "--pull", "never", image, "alloc", mib], env=env,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    line = next((l for l in done.stderr.decode().splitlines() if l.startswith("shards-timing ")), None)
    t = json.loads(line.split(" ", 1)[1]) if line else {}
    age = t.get("request_us", 0) / 1000
    ran = (t.get("answered_us", 0) - t.get("request_us", 0)) / 1000
    print(f"{i} age_ms {age:.0f} status {done.returncode} reported_kib {t.get('reported_kib')} answered_ms {ran:.0f}", flush=True)
