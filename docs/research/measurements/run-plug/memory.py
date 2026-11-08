"""D148: an idle run's memory in two builds, as build-ab/ab.py's DIRs hold them (each its
`shards` and a `home` with IMAGE pulled and the guest recorded): `shards run --rm IMAGE
true` in each in turn, N times, its VM process's footprint and RSS as it ends
(SHARDS_TIMING), n, p50, p90 and max in MiB.

    python3 memory.py OLD_DIR NEW_DIR IMAGE N
"""
import json, os, subprocess, sys

old, new, image, n = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
by = {"old": [], "new": []}
dirs = {"old": old, "new": new}
for i in range(n):
    for arm in ("old", "new") if i % 2 == 0 else ("new", "old"):
        env = dict(os.environ, SHARDS_HOME=os.path.join(dirs[arm], "home"), SHARDS_TIMING="1")
        done = subprocess.run(
            [os.path.join(dirs[arm], "shards"), "run", "--rm", "--pull", "never", image, "true"],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        assert done.returncode == 0, done
        line = next(l for l in done.stderr.decode().splitlines() if l.startswith("shards-timing "))
        t = json.loads(line.split(" ", 1)[1])
        by[arm].append((t["footprint_kib"] / 1024, t["rss_kib"] / 1024))
for arm, v in by.items():
    for i, name in ((0, "footprint"), (1, "rss")):
        s = sorted(x[i] for x in v)
        q = lambda p: s[min(len(s) - 1, len(s) * p // 100)]
        print(f"{arm} {name:9} n={len(s)} p50={q(50):.2f} p90={q(90):.2f} max={s[-1]:.2f} MiB")
