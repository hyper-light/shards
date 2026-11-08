"""PM M430's CLI harness (D214): what `shards plug disk`, `shards unplug disk`, `shards plug
volume` and `shards unplug volume` take, the client's wall clock, spawn to reap, on a
detached run restored from its image's template; N rounds, each step's n, p50, p90, p99
and max in microseconds.

    python3 cli.py SHARDS IMAGE N

Its home and files are made and removed here, its docker config empty; on macOS run it
through macos-sched/launchd-run.sh.
"""
import os, subprocess, sys, tempfile, time

shards, image, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
root = tempfile.mkdtemp()
env = dict(os.environ, SHARDS_HOME=os.path.join(root, "home"), DOCKER_CONFIG=tempfile.mkdtemp())


def sh(*args):
    start = time.perf_counter()
    done = subprocess.run([shards, *args], env=env, stdin=subprocess.DEVNULL, capture_output=True)
    took = (time.perf_counter() - start) * 1e6
    assert done.returncode == 0, done
    return took


sh("pull", image)
sh("run", "--rm", image, "true")
sh("run", "-d", "--name", "plugged", image, "sleep", "3600")
disk = os.path.join(root, "data.img")
sh("plug", "disk", "--size", "64m", "plugged", f"{disk}:/data")
sh("unplug", "disk", "plugged", "/data")
shared = os.path.join(root, "shared")
os.mkdir(shared)
steps = {"plug disk": [], "unplug disk": [], "plug volume": [], "unplug volume": []}
for _ in range(n):
    steps["plug disk"].append(sh("plug", "disk", "plugged", f"{disk}:/data"))
    steps["unplug disk"].append(sh("unplug", "disk", "plugged", "/data"))
    steps["plug volume"].append(sh("plug", "volume", "plugged", f"{shared}:/shared"))
    steps["unplug volume"].append(sh("unplug", "volume", "plugged", "/shared"))
sh("rm", "-f", "plugged")
subprocess.run([shards, "daemon", "stop"], env=env)


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


print("load %.2f %.2f %.2f" % os.getloadavg())
for name, v in steps.items():
    print(f"{name:13} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f} us")
