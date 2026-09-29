"""Keystroke echo through a run's terminal (docs/research/tty-and-interactive-runs.md E1,
platform-measurements.md M32): the time from a byte written to the user's terminal to
its echo read back from it.

Each arm is a session on its own pty, led by a shell, as a terminal window runs one:

- shards: `shards run --rm --pull never -it alpine sh -c 'echo ready; sleep 3600'`, the
  echo made by the guest's pty and carried back through the VM;
- docker: `docker run --rm -it alpine sh -c 'echo ready; sleep 3600'`, through dockerd;
- local: `sh -c 'echo ready; sleep 3600'` on the pty itself, the floor this harness
  measures with: the host kernel echoes.

The arms stay open together and take turns, one keystroke each per round, in rotating
order, 5 ms apart, so that all see the host as it is. The command reads nothing, so each
keystroke's only answer is its echo; ^C ends each arm.

    SHARDS=path/to/shards SHARDS_HOME=... python3 echo.py N
"""
import fcntl, os, select, struct, subprocess, sys, termios, time

N = int(sys.argv[1])
READY = b"echo ready; sleep 3600"
ARMS = {
    "shards": [os.environ["SHARDS"], "run", "--rm", "--pull", "never", "-it", "alpine", "sh", "-c", READY.decode()],
    "docker": ["docker", "run", "--rm", "-it", "alpine", "sh", "-c", READY.decode()],
    "local": ["sh", "-c", READY.decode()],
}


def spawn(argv):
    master, peer = os.openpty()
    fcntl.ioctl(peer, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))

    def leader():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    shell = subprocess.Popen(["/bin/sh", "-c", '"$0" "$@"'] + argv, stdin=peer, stdout=peer,
                             stderr=peer, preexec_fn=leader, close_fds=True)
    os.close(peer)
    return master, shell


def read_until(master, want, deadline):
    got = b""
    while want not in got:
        left = deadline - time.monotonic()
        assert left > 0, (want, got)
        if select.select([master], [], [], left)[0]:
            got += os.read(master, 4096)
    return got


sessions = {name: spawn(argv) for name, argv in ARMS.items()}
for name, (master, _) in sessions.items():
    read_until(master, b"ready\r\n", time.monotonic() + 60)
samples = {name: [] for name in sessions}
names = list(sessions)
for i in range(N + 20):
    for name in names[i % len(names):] + names[: i % len(names)]:
        master, _ = sessions[name]
        start = time.perf_counter_ns()
        os.write(master, b"a")
        read_until(master, b"a", time.monotonic() + 10)
        took = (time.perf_counter_ns() - start) / 1000
        if i >= 20:
            samples[name].append(took)
        time.sleep(0.005)
for master, shell in sessions.values():
    os.write(master, b"\x03")
    shell.wait(timeout=30)


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


print("load %.2f %.2f %.2f" % os.getloadavg())
for name, v in samples.items():
    print(f"{name:7} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f} us")
