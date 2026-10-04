"""TCP through a published port, two builds alternating: whether a change to the network
path costs or saves its bytes anything, apart from what the host is doing
(docs/research/platform-measurements.md M104).

Each DIR holds a build's binaries, as build-ab/ab.py's do, and a `home` with IMAGE in it:
this directory's Dockerfile built, as counters.py says, whose entrypoint is
shards-testguest (crates/testguest): its `serve` workload echoes each connection after a
line of its own. Each arm runs one container of IMAGE,
its port 7000 published on 127.0.0.1. Each turn, both arms in alternating order, one
connection to each sends SIZE MiB while it reads them back, timed from connect to the
echo's end: SIZE MiB each way, host to guest through the published port's connection and
back. Every byte that comes back is checked.

Reported per arm: n, p50, p90, p99 and max of a connection's time, and its throughput
each way at p50; then the median of the turns' paired differences, new less old, with a
bootstrap 95% interval.

    python3 ab.py OLD_DIR NEW_DIR IMAGE N SIZE_MIB
"""
import os, random, socket, statistics, subprocess, sys, threading, time

old, new, image, n, size = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
payload = bytes((i * 7) % 251 for i in range(size << 20))
WARM = 3


def shards(d, *args, check=True):
    env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"))
    done = subprocess.run([os.path.join(d, "shards"), *args], env=env, capture_output=True, text=True)
    if check and done.returncode != 0:
        raise SystemExit(f"{d}: shards {' '.join(args)}: {done.stderr}")
    return done.stdout


def start(d):
    """The arm's echo container, and the host port its 7000 is published on."""
    shards(d, "rm", "-f", "ab-echo", check=False)
    shards(d, "run", "-d", "--name", "ab-echo", "-p", "127.0.0.1::7000",
           "--pull", "never", image, "serve", "7000", str(n + WARM))
    deadline = time.monotonic() + 30
    while "ready" not in shards(d, "logs", "ab-echo"):
        if time.monotonic() > deadline:
            raise SystemExit(f"{d}: the echo never said it was ready")
        time.sleep(0.05)
    return int(shards(d, "port", "ab-echo", "7000").strip().rsplit(":", 1)[1])


def exchange(port):
    """One connection's time, in seconds: SIZE MiB sent while they come back."""
    start = time.perf_counter()
    c = socket.create_connection(("127.0.0.1", port), timeout=60)

    def write():
        c.sendall(payload)
        c.shutdown(socket.SHUT_WR)

    writer = threading.Thread(target=write)
    writer.start()
    got = bytearray()
    while True:
        chunk = c.recv(1 << 20)
        if not chunk:
            break
        got += chunk
    took = time.perf_counter() - start
    writer.join()
    c.close()
    line = got.index(b"\n") + 1
    assert bytes(got[line:]) == payload, f"{len(got) - line} bytes came back of {len(payload)}"
    return took


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


arms = {"old": old, "new": new}
ports = {name: start(d) for name, d in arms.items()}
for name in arms:
    for _ in range(WARM):
        exchange(ports[name])
times = {"old": [], "new": []}
for i in range(n):
    order = ["old", "new"] if i % 2 == 0 else ["new", "old"]
    for name in order:
        times[name].append(exchange(ports[name]))
for d in arms.values():
    shards(d, "rm", "-f", "ab-echo", check=False)
    shards(d, "daemon", "stop", check=False)

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
host = subprocess.run(["uname", "-mrs"], capture_output=True, text=True).stdout.strip()
print(f"host {host}, revision {rev}, {size} MiB each way, n={n} per arm")
for name, xs in times.items():
    ms = [x * 1e3 for x in xs]
    mbit = size * 8 * 1.048576 / statistics.median(xs)
    print(f"{name}: p50 {pct(ms, 50):.1f} ms, p90 {pct(ms, 90):.1f}, p99 {pct(ms, 99):.1f}, "
          f"max {max(ms):.1f}; {mbit:.0f} Mbit/s each way at p50")
diffs = [(b - a) * 1e3 for a, b in zip(times["old"], times["new"])]
boot = sorted(statistics.median(random.choices(diffs, k=len(diffs))) for _ in range(2000))
print(f"new - old, paired: median {statistics.median(diffs):+.1f} ms "
      f"[{boot[50]:+.1f}, {boot[1949]:+.1f}] (95%)")
