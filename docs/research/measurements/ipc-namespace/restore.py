"""What a run's own IPC namespace and /dev/shm cost a microVM (docs/research/
platform-measurements.md M228, D156): the standby makes both before the template's
snapshot, so a restore runs nothing more for them; what it carries is the kernel's objects
for them, kept in the template.

For each INIT given (`NAME=PATH`, a shards-init binary), templates are saved from ROOTFS
(an image's EROFS file) with that init, two of each (`NAME#a`, `NAME#b`), for what
templates of one machine differ by; then each is restored N times, interleaved and their
order rotating each round: `shards restore TEMPLATE -- /bin/true`, timed from spawn to
reap, and split by the timing line SHARDS_TIMING adds (`released_us`, the restore); the
VM process's peak RSS at the timing line (`rss_kib`, KiB) and over its life (wait4's
ru_maxrss: bytes on macOS, KiB on Linux); and its memory as memory per VM is counted, at
the timing line (`footprint_kib`: macOS's phys_footprint, Linux's RssAnon). Each template
is compared with the first, paired by round: the median difference with a bootstrap 95%
interval.

    python3 restore.py SHARDS KERNEL ROOTFS SCRATCH N NAME=INIT...
"""
import json, os, random, shutil, subprocess, sys, time

shards, kernel, rootfs, scratch, n = sys.argv[1:5] + [int(sys.argv[5])]
inits = dict(a.split("=", 1) for a in sys.argv[6:])
env = dict(os.environ, SHARDS_TIMING="1")
templates = {}
for name, init in inits.items():
    for copy in ("a", "b"):
        key = f"{name}#{copy}"
        t = os.path.join(scratch, f"template-{name}-{copy}")
        shutil.rmtree(t, ignore_errors=True)
        saved = subprocess.run(
            [shards, "run", "--kernel", kernel, "--init", init, "--rootfs", rootfs,
             "--snapshot-dir", t, "--no-console"],
            env=env, stdin=subprocess.DEVNULL, capture_output=True)
        assert saved.returncode == 0, saved.stderr.decode()
        templates[key] = t
keys = list(templates)


def restore(template):
    start = time.perf_counter()
    child = subprocess.Popen([shards, "restore", template, "--", "/bin/true"], env=env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.PIPE)
    err = child.stderr.read().decode()
    _, status, usage = os.wait4(child.pid, 0)
    wall = (time.perf_counter() - start) * 1e6
    assert os.waitstatus_to_exitcode(status) == 0, err
    line = next(l for l in err.splitlines() if l.startswith("shards-timing "))
    timing = json.loads(line.split(" ", 1)[1])
    return wall, timing["released_us"], usage.ru_maxrss, timing["rss_kib"], timing["footprint_kib"]


for key in keys:
    restore(templates[key])
samples = {key: [] for key in keys}
for i in range(n):
    order = keys[i % len(keys):] + keys[:i % len(keys)]
    for key in order:
        samples[key].append(restore(templates[key]))


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


rng = random.Random(0)
print("load %.2f %.2f %.2f" % os.getloadavg())
base = keys[0]
for part, of in (("wall", lambda w, r, m, k, f: w), ("released", lambda w, r, m, k, f: r),
                 ("maxrss", lambda w, r, m, k, f: m), ("rss_kib", lambda w, r, m, k, f: k),
                 ("footprint_kib", lambda w, r, m, k, f: f)):
    for key in keys:
        v = [of(*s) for s in samples[key]]
        unit = "" if part in ("maxrss", "rss_kib", "footprint_kib") else " us"
        line = f"{part:13} {key:>12} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f}{unit}"
        if key != base:
            d = [of(*b) - of(*a) for a, b in zip(samples[base], samples[key])]
            boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
            line += f"; less {base}: {q(d, .5):+.0f} [{boot[50]:+.0f}, {boot[1949]:+.0f}]"
        print(line)
