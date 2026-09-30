"""`shards logs --tail 1` of a large log: the daemon's peak RSS and the command's time
(docs/research/platform-measurements.md M48, audit A12).

Each ARM directory holds a build's `shards` and `shardsd` and a `home` with IMAGE pulled
and the guest recorded (build-ab/ab.py's layout). A container is run in each home, its
log replaced by one of SIZE_MIB MiB of 80-byte lines in 64 KiB records, as an earlier
shards wrote it (no index), and `logs --tail 1` is asked for three times, the daemon's RSS
sampled every 5 ms throughout.

    python3 tail.py IMAGE SIZE_MIB ARM...
"""
import os, subprocess, sys, threading, time

image, size_mib, arms = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
line = b"x" * 79 + b"\n"
payload = line * (65536 // len(line))
record = bytes([1]) + (0).to_bytes(8, "big") + len(payload).to_bytes(4, "big") + payload


def rss_kib(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout
    return int(out.strip() or 0)


for arm in arms:
    env = dict(os.environ, SHARDS_HOME=os.path.join(arm, "home"))
    shards = os.path.join(arm, "shards")
    subprocess.run([shards, "rm", "-f", "big"], env=env, capture_output=True)
    run = subprocess.run([shards, "run", "--name", "big", "--pull", "never", image, "true"], env=env, capture_output=True)
    assert run.returncode == 0, run
    listed = subprocess.run([shards, "ps", "-a", "--no-trunc"], env=env, capture_output=True, text=True).stdout
    cid = next(l.split()[0] for l in listed.splitlines() if l.endswith(" big"))
    d = os.path.join(arm, "home", "containers", cid)
    with open(os.path.join(d, "log"), "wb") as f:
        for _ in range(size_mib * 1024 * 1024 // len(record)):
            f.write(record)
    if os.path.exists(os.path.join(d, "log.idx")):
        os.remove(os.path.join(d, "log.idx"))
    pid = int(open(os.path.join(arm, "home", "daemon.pid")).read())
    for attempt in range(3):
        peak, done = [rss_kib(pid)], threading.Event()
        base = peak[0]

        def sample():
            while not done.is_set():
                peak.append(rss_kib(pid))
                time.sleep(0.005)

        t = threading.Thread(target=sample)
        t.start()
        t0 = time.perf_counter()
        out = subprocess.run([shards, "logs", "--tail", "1", "big"], env=env, capture_output=True)
        took = time.perf_counter() - t0
        done.set()
        t.join()
        assert out.returncode == 0 and out.stdout == line, (out.returncode, out.stdout[:100], out.stderr[:300])
        print(f"{os.path.basename(arm)} {size_mib} MiB attempt {attempt}: {took * 1000:.0f} ms, "
              f"daemon RSS {base / 1024:.0f} MiB before, {max(peak) / 1024:.0f} MiB at peak", flush=True)
    subprocess.run([shards, "rm", "-f", "big"], env=env, capture_output=True)
