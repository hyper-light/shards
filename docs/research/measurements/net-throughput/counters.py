"""What the guest's own TCP saw of a build's network path: N connections through a
published port, each SIZE MiB each way, as ab.py's, then the guest kernel's counters
(/proc/net/netstat, /proc/net/snmp, the interface's statistics) before and after, for
every one that moved (docs/research/platform-measurements.md M104).

IMAGE is this directory's Dockerfile built: Alpine, for `cat`, and shards-testguest as
its entrypoint, whose `serve` echoes each connection after a line of its own:

    cargo build -p shards-testguest --profile guest --target <arch>-unknown-linux-musl
    cp target/<arch>-unknown-linux-musl/guest/shards-testguest DIR/testguest
    cp Dockerfile DIR/ && shards build -t nettest DIR

Every byte that comes back is checked: an echo cut short or changed is counted and
said, with where it first differs; so is a connection that took over 100 ms.

    python3 counters.py BUILD_DIR IMAGE N SIZE_MIB
"""
import os, socket, subprocess, sys, threading, time

d, image, n, size = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
payload = bytes((i * 7) % 251 for i in range(size << 20))
env = dict(os.environ, SHARDS_HOME=os.path.join(d, "home"))


def shards(*args, check=True):
    done = subprocess.run([os.path.join(d, "shards"), *args], env=env, capture_output=True, text=True)
    if check and done.returncode != 0:
        raise SystemExit(f"shards {' '.join(args)}: {done.stderr}")
    return done.stdout


def counters():
    out = {}
    for f in ("/proc/net/netstat", "/proc/net/snmp"):
        lines = shards("exec", "ab-echo", "cat", f).splitlines()
        for names, values in zip(lines[0::2], lines[1::2]):
            group = names.split()[0]
            for k, v in zip(names.split()[1:], values.split()[1:]):
                out[group + k] = int(v)
    for f in ("rx_packets", "rx_dropped", "rx_errors", "rx_length_errors", "tx_packets", "tx_dropped"):
        out["eth0." + f] = int(shards("exec", "ab-echo", "cat", "/sys/class/net/eth0/statistics/" + f))
    return out


shards("rm", "-f", "ab-echo", check=False)
shards("run", "-d", "--name", "ab-echo", "-p", "127.0.0.1::7000", "--pull", "never",
       image, "serve", "7000", str(n + 1))
deadline = time.monotonic() + 30
while "ready" not in shards("logs", "ab-echo"):
    if time.monotonic() > deadline:
        raise SystemExit("the echo never said it was ready")
    time.sleep(0.05)
port = int(shards("port", "ab-echo", "7000").strip().rsplit(":", 1)[1])
before = counters()
slow = wrong = 0
for i in range(n):
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
    back = bytes(got[got.index(b"\n") + 1:]) if b"\n" in got else b""
    if back != payload:
        wrong += 1
        at = next((j for j in range(min(len(back), len(payload))) if back[j] != payload[j]),
                  min(len(back), len(payload)))
        print(f"connection {i}: {len(back)} bytes came back of {len(payload)}, first differing at {at}")
    if took > 0.1:
        slow += 1
after = counters()
shards("rm", "-f", "ab-echo", check=False)
shards("daemon", "stop", check=False)

rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
host = subprocess.run(["uname", "-mrs"], capture_output=True, text=True).stdout.strip()
print(f"host {host}, revision {rev}: {n} connections of {size} MiB each way, "
      f"{wrong} cut short or changed, {slow} over 100 ms")
for k in sorted(after):
    if after[k] != before.get(k, 0):
        print(f"  {k} +{after[k] - before.get(k, 0)}")
