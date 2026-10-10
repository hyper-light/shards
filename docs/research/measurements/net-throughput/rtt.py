"""Round trips through a published port, two builds alternating: whether a change to the
network path costs a round trip anything, apart from what the host is doing
(docs/research/platform-measurements.md M137).

Each DIR holds a build's binaries and a `home` with IMAGE in it, as ab.py's do. Each arm
runs one container of IMAGE whose `hold` workload echoes what each connection sends, its
port 7000 published on 127.0.0.1, and holds one connection to it (TCP_NODELAY). Each
turn, both arms in alternating order, each sends SIZE bytes and waits for them back, K
times: one round trip is one send to its last byte back, checked.

Reported per arm: n, p50, p90, p99 and max of a round trip; then the median of the
turns' paired differences (each turn's median, new less old), with a bootstrap 95%
interval.

AB_DUMP names a file for every round trip, in microseconds, per arm in turn order, as
JSON.

    python3 rtt.py OLD_DIR NEW_DIR IMAGE TURNS K SIZE
"""
import json, os, random, socket, statistics, subprocess, sys, time

old, new, image = sys.argv[1], sys.argv[2], sys.argv[3]
turns, k, size = int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
payload = bytes((i * 7) % 251 for i in range(size))
WARM = 200


def shards(d, *args, check=True):
    env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"))
    done = subprocess.run([os.path.join(d, "shards"), *args], env=env, capture_output=True, text=True)
    if check and done.returncode != 0:
        raise SystemExit(f"{d}: shards {' '.join(args)}: {done.stderr}")
    return done.stdout


def start(d):
    """The arm's connection to its echo."""
    shards(d, "rm", "-f", "ab-hold", check=False)
    shards(d, "run", "-d", "--name", "ab-hold", "-p", "127.0.0.1::7000",
           "--pull", "never", image, "hold", "7000")
    deadline = time.monotonic() + 30
    while "ready" not in shards(d, "logs", "ab-hold"):
        if time.monotonic() > deadline:
            raise SystemExit(f"{d}: the echo never said it was ready")
        time.sleep(0.05)
    port = int(shards(d, "port", "ab-hold", "7000").strip().rsplit(":", 1)[1])
    c = socket.create_connection(("127.0.0.1", port), timeout=30)
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return c


def round_trip(c):
    """One round trip's time, in microseconds."""
    t = time.perf_counter()
    c.sendall(payload)
    got = bytearray()
    while len(got) < size:
        chunk = c.recv(size - len(got))
        if not chunk:
            raise SystemExit("the echo closed the connection")
        got += chunk
    took = (time.perf_counter() - t) * 1e6
    assert bytes(got) == payload, "the echo came back changed"
    return took


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


arms = {"old": old, "new": new}
conns = {name: start(d) for name, d in arms.items()}
for name in arms:
    for _ in range(WARM):
        round_trip(conns[name])
times = {"old": [], "new": []}
medians = {"old": [], "new": []}
for i in range(turns):
    order = ["old", "new"] if i % 2 == 0 else ["new", "old"]
    for name in order:
        xs = [round_trip(conns[name]) for _ in range(k)]
        times[name] += xs
        medians[name].append(statistics.median(xs))
for name, d in arms.items():
    conns[name].close()
    shards(d, "rm", "-f", "ab-hold", check=False)
    shards(d, "daemon", "stop", check=False)
if os.environ.get("AB_DUMP"):
    with open(os.environ["AB_DUMP"], "w") as f:
        json.dump(times, f)

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
host = subprocess.run(["uname", "-mrs"], capture_output=True, text=True).stdout.strip()
print(f"host {host}, revision {rev}, {size} bytes each way, {turns} turns of {k}: "
      f"n={turns * k} per arm")
for name, xs in times.items():
    print(f"{name}: p50 {pct(xs, 50):.1f} us, p90 {pct(xs, 90):.1f}, p99 {pct(xs, 99):.1f}, "
          f"max {max(xs):.1f}")
diffs = [b - a for a, b in zip(medians["old"], medians["new"])]
boot = sorted(statistics.median(random.choices(diffs, k=len(diffs))) for _ in range(2000))
print(f"new - old, paired turns: median {statistics.median(diffs):+.1f} us "
      f"[{boot[50]:+.1f}, {boot[1949]:+.1f}] (95%)")
