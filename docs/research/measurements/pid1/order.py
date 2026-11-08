"""The order of dockerd's stop and die events (PM M145), repeated, where SIGTERM ends the command
(--init) and where stop's timeout ends it with SIGKILL (PID 1). Only w-d115-* containers,
each removed."""
import subprocess
import sys
import time

N = int(sys.argv[1]) if len(sys.argv) > 1 else 10
DIND = ["docker", "exec", "shards-dind", "docker"]


def d(*argv):
    return subprocess.run(DIND + list(argv), capture_output=True, text=True, timeout=120)


for case, flags in [("term", ["--init"]), ("timeout", [])]:
    orders = {}
    for i in range(N):
        name = f"w-d115-order-{case}-{i}"
        since = str(int(time.time()) - 1)
        d("run", "-d", "--name", name, *flags, "alpine:3.22", "sleep", "1000")
        time.sleep(0.3)
        d("stop", "-t", "1", name)
        d("rm", name)
        until = str(int(time.time()) + 1)
        ev = d("events", "--since", since, "--until", until, "--filter", f"container={name}", "--format", "{{.Action}}")
        seq = tuple(a for a in ev.stdout.split() if a in ("kill", "die", "stop"))
        orders[seq] = orders.get(seq, 0) + 1
    for seq, n in sorted(orders.items(), key=lambda x: -x[1]):
        print(f"{case}: {n}/{N} {' '.join(seq)}", flush=True)
