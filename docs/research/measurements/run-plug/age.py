"""D151: whether a cost follows a warm VM's age. Runs `shards run --rm --pull never IMAGE
true` N times through DIR's `shards` and `home` (as build-ab/ab.py's DIRs), a pause after
each as ab.py's, and reads from each run's timing line (SHARDS_TIMING) the age of the VM
that took it as its request came (`request_us`, from its process's start) and its
command's time (`answered_us` less `request_us`). Prints them by half second of age:
n, p50 and max of the command's time.

    python3 age.py DIR IMAGE N
"""
import json, os, statistics, subprocess, sys, time

d, image, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
pause = float(os.environ.get("AB_PAUSE", "0.3"))
rows = []
for _ in range(n):
    env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"), SHARDS_TIMING="1")
    done = subprocess.run(
        [os.path.join(d, "shards"), "run", "--rm", "--pull", "never", image, "true"],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    line = next((l for l in done.stderr.decode().splitlines() if l.startswith("shards-timing ")), None)
    if line:
        t = json.loads(line.split(" ", 1)[1])
        rows.append((t["request_us"], t["answered_us"] - t["request_us"]))
    time.sleep(pause)
by_age = {}
for age, command in rows:
    by_age.setdefault(min(age // 500_000, 8), []).append(command)
print(f"n {len(rows)} command p50 {statistics.median(c for _, c in rows):.0f} us")
for half, v in sorted(by_age.items()):
    print(f"  age {half / 2:.1f}-{half / 2 + 0.5:.1f} s: n {len(v)} p50 {statistics.median(v):.0f} max {max(v)} us")
