#!/usr/bin/env python3
"""A cold run's time, and where its outliers go (kiln's sandbox_launch on the Mac: 3 of 20
cold runs at 2.3 to 6.8 s where the median was 218 ms): N runs of IMAGE with a pool of
none (SHARDS_POOL=0), each restoring a microVM of its own from the image's template, each
timed by its client, with its SHARDS_TIMING line and what its daemon's log said while it
ran (SHARDS_LOG=debug). With --assess K, a thread meanwhile launches K new executables a
second, a small program copied to a path of its own each time, as a build makes them,
which macOS assesses before it lets them run, and a launch of any other executable waits
behind those assessments (PM M166).

    cold.py SHARDS IMAGE N OUT [--seed HOME] [--assess K] [--arm NAME=SHARDS]...
            [--log LEVEL] [--block K] [-- CMD...]

The home is OUT/home, given SEED's image store and guest kernel by clone where --seed
names a home that holds IMAGE, or pulling it; its first run, which saves the template, is
not counted. DOCKER_CONFIG is an empty directory of its own. Into OUT/runs.jsonl, each
run: its seconds, its exit, its timing line, and its daemon's log lines while it ran.
Prints n, p50, p90, p99 and max of the runs' milliseconds with host, OS, load and
revision, and each run past three times the median with what its daemon said. With
--arm, each NAME's build of shards is an arm of its own beside SHARDS's (`a`), with a home
of its own (OUT/home-NAME), the arms' runs interleaved, each round's first in turn, so
that each meets the host as the others do; with --block K, K runs of an arm at a time, so
that what one arm's runs leave the host doing (a flush, a write-back) reaches its own
runs, not another's. --log sets SHARDS_LOG (debug by default; `none` leaves it unset, as
a user's runs have it). `no-flush.patch` is a build whose writable layer answers its
guest's flushes at once, nothing made durable: the other arm of the layer flush's A/B.
"""
import json, os, platform, shutil, subprocess, sys, tempfile, threading, time

args = sys.argv[1:]
cmd = ["true"]
if "--" in args:
    cmd = args[args.index("--") + 1 :]
    args = args[: args.index("--")]
seed, assess, log_level, extra, block = None, 0, "debug", [], 1
if "--block" in args:
    i = args.index("--block")
    block = int(args[i + 1])
    del args[i : i + 2]
while "--arm" in args:
    i = args.index("--arm")
    name, _, build = args[i + 1].partition("=")
    extra.append((name, os.path.realpath(build)))
    del args[i : i + 2]
if "--log" in args:
    i = args.index("--log")
    log_level = args[i + 1]
    del args[i : i + 2]
if "--seed" in args:
    i = args.index("--seed")
    seed = args[i + 1]
    del args[i : i + 2]
if "--assess" in args:
    i = args.index("--assess")
    assess = float(args[i + 1])
    del args[i : i + 2]
shards, image, n, out = os.path.realpath(args[0]), args[1], int(args[2]), os.path.realpath(args[3])
os.makedirs(out, exist_ok=True)
config = tempfile.mkdtemp(prefix="cold-config-")
arms = [("a", shards)] + extra
homes = {name: os.path.join(out, "home" if not extra else f"home-{name}") for name, _ in arms}
builds = dict(arms)


def env(arm):
    e = dict(os.environ, SHARDS_HOME=homes[arm], DOCKER_CONFIG=config, SHARDS_POOL="0", SHARDS_TIMING="1")
    e.pop("SHARDS_LOG", None)
    if log_level != "none":
        e["SHARDS_LOG"] = log_level
    return e


def shards_run(arm, *argv, timeout=600):
    return subprocess.run([builds[arm], *argv], env=env(arm), capture_output=True, text=True, timeout=timeout)


def clean():
    for arm in builds:
        subprocess.run([builds[arm], "daemon", "stop"], env=env(arm), capture_output=True)
    shutil.rmtree(config, ignore_errors=True)


for arm in builds:
    os.makedirs(homes[arm], exist_ok=True)
    if seed:
        for part in ("images", "guest"):
            src = os.path.join(seed, part)
            if os.path.isdir(src) and not os.path.isdir(os.path.join(homes[arm], part)):
                subprocess.run(["cp", "-cR", src, os.path.join(homes[arm], part)], check=True)
    else:
        done = shards_run(arm, "pull", "-q", image)
        if done.returncode != 0:
            clean()
            raise SystemExit(f"pull: {done.stderr}")
try:
    for arm in builds:
        first = shards_run(arm, "run", "--rm", "--pull", "never", image, *cmd)
        if first.returncode != 0:
            raise SystemExit(f"{arm}: the first run: {first.stderr}")

    # What macOS assesses meanwhile: a small program of this host's compiler, ad-hoc signed
    # as the linker signs every arm64 executable, launched from a new path each time.
    stop = threading.Event()
    assessed = [0]
    if assess:
        source = os.path.join(out, "tiny.c")
        with open(source, "w") as f:
            f.write("int main(void) { return 0; }\n")
        tiny = os.path.join(out, "tiny")
        subprocess.run(["cc", "-O2", "-o", tiny, source], check=True)

        def assessor(w):
            k = 0
            while not stop.is_set():
                began = time.monotonic()
                path = os.path.join(out, "assess", str(w), str(k), "tiny")
                os.makedirs(os.path.dirname(path), exist_ok=True)
                shutil.copy2(tiny, path)
                subprocess.Popen([path]).wait()
                assessed[0] += 1
                k += 1
                left = len(workers) / assess - (time.monotonic() - began)
                if left > 0:
                    stop.wait(left)

        workers = [threading.Thread(target=assessor, args=(w,)) for w in range(max(1, int(assess // 4)))]
        for w in workers:
            w.start()

    runs = []
    names = [name for name, _ in arms]
    with open(os.path.join(out, "runs.jsonl"), "w") as f:
        for start in range(0, n, block):
            turn = start // block
            for arm, i in [(arm, i) for arm in names[turn % len(names):] + names[: turn % len(names)]
                           for i in range(start, min(n, start + block))]:
                log = os.path.join(homes[arm], "daemon.log")
                seen = os.path.getsize(log) if os.path.exists(log) else 0
                began = time.monotonic()
                done = shards_run(arm, "run", "--rm", "--pull", "never", image, *cmd)
                took = time.monotonic() - began
                with open(log, errors="replace") as d:
                    d.seek(seen)
                    said = d.read().splitlines()
                timing = next((json.loads(l.split(" ", 1)[1]) for l in done.stderr.splitlines()
                               if l.startswith("shards-timing ")), None)
                run = {"arm": arm, "i": i, "s": took, "exit": done.returncode, "timing": timing,
                       "said": said, "load": os.getloadavg()[0]}
                runs.append(run)
                f.write(json.dumps(run) + "\n")
                f.flush()
    stop.set()
    if assess:
        for w in workers:
            w.join()
finally:
    clean()


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
print(f"host {platform.node()} {platform.machine()}, {platform.system()} {platform.release()}, "
      f"{os.cpu_count()} CPUs, revision {rev}; {image} {' '.join(cmd)}; assessing {assess}/s "
      f"({assessed[0]} launched); load {min(r['load'] for r in runs):.1f} to {max(r['load'] for r in runs):.1f}")
for name, build in arms:
    mine = [r for r in runs if r["arm"] == name]
    ms = [r["s"] * 1000 for r in mine]
    print(f"{name} ({build}): cold runs n {len(ms)} p50 {pct(ms, .5):.1f} p90 {pct(ms, .9):.1f} "
          f"p99 {pct(ms, .99):.1f} max {max(ms):.1f} ms; failed {sum(1 for r in mine if r['exit'] != 0)}")
    median = pct(ms, .5)
    for r in mine:
        if r["s"] * 1000 > 3 * median:
            print(f"  {name} run {r['i']}: {r['s'] * 1000:.0f} ms, exit {r['exit']}, load {r['load']:.1f}")
            for line in r["said"][-12:]:
                print(f"    {line[:220]}")
