#!/usr/bin/env python3
"""Where a pooled run's time goes, request to usable, across the processes it passes
through: the client, the daemon's slot and handling threads, the network process, the
warm VM's process and its guest.

Takes a `shards` built with trace.patch, which makes every process's timing clock the
host's monotonic clock (`shards_vmm::log::uptime_us`), adds the client's own readings to
the run's timing line (`client_main_us` as its `main` began, `client_asked_us` once its
request was written to its slot) and has the daemon write a `shards-trace PID STEP US`
line to its log at each step of a run (its slot opened and its request read, its
handling's start, a warm VM acquired, its network process told, the VM's TAKEN, the
command's EXIT passed on). This harness reads the same clock (CLOCK_MONOTONIC) as it
spawns each client and reaps it.

HOME holds IMAGE already. Each run is `shards run --pull never [FLAGS] IMAGE COMMAND`
with SHARDS_TIMING set and an empty DOCKER_CONFIG, then a pause (--pause, seconds) in
which the daemon refills its pool. --warmup runs are not counted: the first saves the
image's template and the next ones record its working set (PM M30, M33).

Steps, each from the reading before it:
- `launch`: the spawn to the client's `main`;
- `client`: to its request written to its slot;
- `slot`: to the daemon's slot thread having read it whole;
- `bridge`: to the daemon's handling of it starting;
- `acquire`: to a warm VM in hand;
- `net`: to its network process told the run's network (E1);
- `hand`: to the VM process having the run (its `request_us`);
- `guest`: to the command executing in the guest (WORKLOAD_STARTED): `usable` ends here;
- `command`: to the command's exit (WORKLOAD_EXITED);
- `answer`: to its VM process having read its status;
- `back`: to the client having the status (`client_us`);
- `exit`: to the client reaped.

`usable` is the spawn to WORKLOAD_STARTED; `wall` the spawn to the reap.

    trace.py SHARDS HOME IMAGE N [--warmup W] [--pause S] [--flags "..."] [--dump F] [COMMAND...]

Prints n, p50, p90, p99 and max of each, ms, with host, OS, CPUs and load."""
import argparse, json, os, platform, statistics, subprocess, sys, tempfile, time

p = argparse.ArgumentParser()
p.add_argument("shards")
p.add_argument("home")
p.add_argument("image")
p.add_argument("n", type=int)
p.add_argument("--warmup", type=int, default=5)
p.add_argument("--pause", type=float, default=0.3)
p.add_argument("--flags", default="")
p.add_argument("--dump")
p.add_argument("command", nargs="*")
a = p.parse_args()
command = a.command or ["exit", "0"]

STARTED, EXITED = 4, 5
env = dict(os.environ, SHARDS_HOME=a.home, SHARDS_TIMING="1", DOCKER_CONFIG=tempfile.mkdtemp())
log_path = os.path.join(a.home, "daemon.log")


def now_us():
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC) // 1000


def log_size():
    try:
        return os.path.getsize(log_path)
    except FileNotFoundError:
        return 0


def one():
    at = log_size()
    spawn = now_us()
    done = subprocess.run(
        [a.shards, "run", "--pull", "never", *a.flags.split(), a.image, *command],
        env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    reap = now_us()
    err = done.stderr.decode(errors="replace")
    line = next((l for l in err.splitlines() if l.startswith("shards-timing ")), None)
    if done.returncode != 0 or line is None:
        sys.exit(f"run failed ({done.returncode}): {err}")
    t = json.loads(line.split(" ", 1)[1])
    markers = dict((m, at_) for m, at_ in t["markers"])
    steps = {}
    with open(log_path, errors="replace") as f:
        f.seek(at)
        for l in f:
            if l.startswith("shards-trace "):
                _, _pid, step, us = l.split()
                us = int(us)
                # The first of each step within this run's span: refills' and other runs' fall outside.
                if spawn <= us <= reap and step not in steps:
                    steps[step] = us
    points = [
        ("launch", t.get("client_main_us")),
        ("client", t.get("client_asked_us")),
        ("slot", steps.get("slot_request")),
        ("bridge", steps.get("handle_start")),
        ("acquire", steps.get("run_acquired")),
        ("net", steps.get("run_told_net")),
        ("hand", t.get("request_us")),
        ("guest", markers.get(STARTED)),
        ("command", markers.get(EXITED)),
        ("answer", t.get("answered_us")),
        ("back", t.get("client_us")),
        ("exit", reap),
    ]
    row, last = {}, spawn
    for name, us in points:
        if not us:
            row[name] = None
            continue
        row[name] = (us - last) / 1000
        last = us
    row["usable"] = (markers[STARTED] - spawn) / 1000 if markers.get(STARTED) else None
    row["wall"] = (reap - spawn) / 1000
    # The hand-over's own steps: the run sent, the VM's read of it, its taking, its
    # TAKEN read by the daemon, and the rest of its preparing (the `hand` step's end).
    within = [("run_told_net", "h_send", "run_sent"), ("run_sent", "h_vm_read", "vm_recv"),
              ("vm_recv", "h_vm_decode", "vm_taking"), ("vm_taking", "h_taken_read", "run_taken")]
    for begin, name, end in within:
        row[name] = (steps[end] - steps[begin]) / 1000 if begin in steps and end in steps else None
    row["h_vm_prepare"] = (t["request_us"] - steps["vm_taking"]) / 1000 if "vm_taking" in steps else None
    return row


def pct(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, max(0, round(q / 100 * len(v) + 0.5) - 1))]


for _ in range(a.warmup):
    one()
    time.sleep(max(a.pause, 1.0))
rows = []
for _ in range(a.n):
    rows.append(one())
    time.sleep(a.pause)
if a.dump:
    with open(a.dump, "w") as f:
        json.dump(rows, f)
rev = subprocess.run([a.shards, "--version"], capture_output=True, text=True).stdout.strip()
print(f"host: {platform.node()} {platform.machine()} cpus {os.cpu_count()}; os: {platform.platform()}")
print(f"shards: {rev}; image {a.image}; command {' '.join(command)}; flags {a.flags!r}; load {os.getloadavg()}")
print(f"{'step':<17}{'n':>5}{'p50':>9}{'p90':>9}{'p99':>9}{'max':>9}  ms")
for name in rows[0]:
    v = [r[name] for r in rows if r[name] is not None]
    if not v:
        print(f"{name:<17} none")
        continue
    print(f"{name:<17}{len(v):>5}{pct(v, 50):>9.3f}{pct(v, 90):>9.3f}{pct(v, 99):>9.3f}{max(v):>9.3f}")
