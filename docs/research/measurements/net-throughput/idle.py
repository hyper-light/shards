"""What a connection's round trip costs as the connections a VM's network process holds
grow (review 2.14, docs/research/platform-measurements.md M106): N connections through
a published port that the guest holds and nothing uses, beside one that sends 64 bytes
and waits for them back, K times.

IMAGE is this directory's Dockerfile built, as counters.py says: its entrypoint
shards-testguest, whose `hold` workload holds every connection it is given (on epoll,
so that the ones it holds cost it nothing an event) and echoes what each sends.

    python3 idle.py BUILD_DIR IMAGE K N...
"""
import os, socket, subprocess, sys, time

d, image, k = sys.argv[1], sys.argv[2], int(sys.argv[3])
counts = [int(n) for n in sys.argv[4:]] or [0, 250, 1000, 3500]
env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"))


def shards(*args, check=True):
    done = subprocess.run([os.path.join(d, "shards"), *args], env=env, capture_output=True, text=True)
    if check and done.returncode != 0:
        raise SystemExit(f"shards {' '.join(args)}: {done.stderr}")
    return done.stdout


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
host = subprocess.run(["uname", "-mrs"], capture_output=True, text=True).stdout.strip()
print(f"host {host}, revision {rev}: {k} round trips of 64 bytes beside N idle connections")
for n in counts:
    shards("rm", "-f", "hold", check=False)
    shards("run", "-d", "--name", "hold", "-p", "127.0.0.1::7000", "--pull", "never", image, "hold", "7000")
    deadline = time.monotonic() + 30
    while "ready" not in shards("logs", "hold"):
        if time.monotonic() > deadline:
            raise SystemExit("the guest never said it was ready")
        time.sleep(0.05)
    port = int(shards("port", "hold", "7000").strip().rsplit(":", 1)[1])
    talk = socket.create_connection(("127.0.0.1", port), timeout=30)
    talk.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    idle = [socket.create_connection(("127.0.0.1", port), timeout=30) for _ in range(n)]
    # Every idle connection through to the guest before the clock starts: each echoes once.
    for c in idle:
        c.sendall(b"!")
    for c in idle:
        c.recv(1)
    out, rtt = b"x" * 64, []
    for _ in range(k):
        t = time.perf_counter()
        talk.sendall(out)
        got = 0
        while got < len(out):
            got += len(talk.recv(64))
        rtt.append((time.perf_counter() - t) * 1e6)
    print(f"N={n}: p50 {pct(rtt, 50):.0f} us, p90 {pct(rtt, 90):.0f}, p99 {pct(rtt, 99):.0f}, max {max(rtt):.0f}")
    for c in idle + [talk]:
        c.close()
    shards("rm", "-f", "hold", check=False)
shards("daemon", "stop", check=False)
