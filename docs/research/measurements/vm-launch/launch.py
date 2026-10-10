# How long a VM process in App Sandbox takes from its spawn to its first request for
# access (PM M166): the first thing a VM does once it runs is ask its spawner for its
# files (vm_run.rs `start` → `confine` → `granted`), so the wait is its launch, App
# Sandbox's set-up included. Each launch is a `shards-vm run --kernel K --grants 3` given
# a socket at descriptor 3, timed to the first byte on it, and ended. A launch slower than
# THRESHOLD seconds is sampled (sample(1)) while it is still waiting, so that a stall says
# where it is. Run under the host's ordinary load: busy hosts are the norm.
# With FRESH set to 1, each launch is of a new copy of VM_BINARY (a path and file of its
# own, the same bytes and signature), as each new build's VM process is the first time it
# runs (shards writes its helpers out by digest, helpers.rs).
#   python3 -I launch.py VM_BINARY KERNEL N CONCURRENCY OUT_DIR [THRESHOLD [FRESH]]
import json, os, select, shutil, signal, socket, subprocess, sys, threading, time

vm_given, kernel, n, workers, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
threshold = float(sys.argv[6]) if len(sys.argv) > 6 else 1.0
fresh = len(sys.argv) > 7 and sys.argv[7] == "1"
os.makedirs(out, exist_ok=True)
lock = threading.Lock()
results = []
counter = iter(range(n))

def launch(i):
    vm = vm_given
    if fresh:
        copies = os.path.join(out, "copies", str(i))
        os.makedirs(copies, exist_ok=True)
        vm = os.path.join(copies, "shards-vm")
        shutil.copy2(vm_given, vm)
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    actions = [(os.POSIX_SPAWN_DUP2, theirs.fileno(), 3)]
    began = time.monotonic()
    pid = os.posix_spawn(vm, [vm, "run", "--kernel", kernel, "--grants", "3"], {}, file_actions=actions)
    theirs.close()
    sampled = None
    waited = None
    deadline = began + 60.0
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            break
        step = min(left, max(threshold - (time.monotonic() - began), 0.001)) if sampled is None else left
        ready, _, _ = select.select([ours], [], [], step)
        if ready:
            waited = time.monotonic() - began
            break
        if sampled is None and time.monotonic() - began >= threshold:
            sampled = os.path.join(out, f"sample-{pid}.txt")
            subprocess.Popen(["/usr/bin/sample", str(pid), "2", "-mayDie", "-file", sampled],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)
    ours.close()
    return waited, sampled

def launch_again(i):
    vm = os.path.join(out, "copies", str(i), "shards-vm")
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    began = time.monotonic()
    pid = os.posix_spawn(vm, [vm, "run", "--kernel", kernel, "--grants", "3"], {},
                         file_actions=[(os.POSIX_SPAWN_DUP2, theirs.fileno(), 3)])
    theirs.close()
    ready, _, _ = select.select([ours], [], [], 60.0)
    waited = time.monotonic() - began if ready else None
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)
    ours.close()
    return waited

def worker():
    while True:
        with lock:
            i = next(counter, None)
        if i is None:
            return
        waited, sampled = launch(i)
        # A fresh copy launched a second time: what its first launch paid that a file
        # launched before does not.
        again = launch_again(i) if fresh else None
        with lock:
            results.append({"i": i, "s": waited, "sample": sampled, "again": again})

if len(sys.argv) > 7 and sys.argv[7] == "herd":
    # Rounds of one fresh copy launched by every worker at once, as a new build's VM
    # process is by the VMs its first runs start together: N is the rounds.
    rounds = []
    for r in range(n):
        copies = os.path.join(out, "herd", str(r))
        os.makedirs(copies, exist_ok=True)
        copy = os.path.join(copies, "shards-vm")
        shutil.copy2(vm_given, copy)
        barrier = threading.Barrier(workers)
        times = []
        def one():
            ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
            barrier.wait()
            began = time.monotonic()
            pid = os.posix_spawn(copy, [copy, "run", "--kernel", kernel, "--grants", "3"], {},
                                 file_actions=[(os.POSIX_SPAWN_DUP2, theirs.fileno(), 3)])
            theirs.close()
            ready, _, _ = select.select([ours], [], [], 120.0)
            waited = time.monotonic() - began if ready else None
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
            ours.close()
            with lock:
                times.append(waited)
        ts = [threading.Thread(target=one) for _ in range(workers)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        rounds.append(sorted(t for t in times if t is not None))
    flat = sorted(t for r in rounds for t in r)
    summary = {
        "rounds": n, "workers": workers, "load": os.getloadavg(),
        "p50_ms": flat[len(flat) // 2] * 1000, "p90_ms": flat[int(0.9 * len(flat))] * 1000,
        "p99_ms": flat[min(len(flat) - 1, int(0.99 * len(flat)))] * 1000, "max_ms": flat[-1] * 1000,
        "first_of_round_ms": [r[0] * 1000 for r in rounds if r],
        "last_of_round_ms": [r[-1] * 1000 for r in rounds if r],
    }
    json.dump(summary, open(os.path.join(out, "herd.json"), "w"), indent=1)
    print(json.dumps(summary, indent=1))
    sys.exit(0)

load = os.getloadavg()
threads = [threading.Thread(target=worker) for _ in range(workers)]
for t in threads:
    t.start()
for t in threads:
    t.join()
done = sorted(r["s"] for r in results if r["s"] is not None)
stuck = [r for r in results if r["s"] is None]
def q(p):
    return done[min(len(done) - 1, int(p * len(done)))] * 1000 if done else None
summary = {
    "n": len(results), "answered": len(done), "never": len(stuck), "workers": workers, "fresh": fresh,
    "load_before": load, "load_after": os.getloadavg(),
    "p50_ms": q(0.50), "p90_ms": q(0.90), "p99_ms": q(0.99), "max_ms": done[-1] * 1000 if done else None,
    "over_threshold": sum(1 for s in done if s >= threshold),
    "samples": [r["sample"] for r in results if r["sample"]],
}
if fresh:
    again = sorted(r["again"] for r in results if r["again"] is not None)
    summary["again_p50_ms"] = again[len(again) // 2] * 1000 if again else None
    summary["again_p99_ms"] = again[min(len(again) - 1, int(0.99 * len(again)))] * 1000 if again else None
    summary["again_max_ms"] = again[-1] * 1000 if again else None
json.dump({"summary": summary, "launches": results}, open(os.path.join(out, "results.json"), "w"), indent=1)
print(json.dumps(summary, indent=1))
