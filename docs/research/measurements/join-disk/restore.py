"""What a restore costs a microVM for each empty disk it carries (docs/research/
platform-measurements.md M168): the price of a join disk in every microVM, empty until a
container joins its network (D119).

For each VARIANT, a template is saved from ROOTFS (an image's EROFS file): a count, with
that many empty read-only virtio-blk disks, or `join`, with a join disk and none; then
each is restored N times, the variants interleaved and their order rotating each round:
`shards restore TEMPLATE -- /bin/true`, timed from spawn to reap, and split by the timing
line SHARDS_TIMING adds: `released_us`, the VM process's start to its vCPUs' release (the
restore), and the rest. Each variant is compared with the first, paired by round: the
median difference with a bootstrap 95% interval.

    python3 restore.py SHARDS KERNEL INIT ROOTFS SCRATCH N VARIANT...
"""
import json, os, random, shutil, subprocess, sys, time

shards, kernel, init, rootfs, scratch, n = sys.argv[1:6] + [int(sys.argv[6])]
counts = sys.argv[7:] or ["0", "1"]
env = dict(os.environ, SHARDS_TIMING="1")
templates = {}
for count in counts:
    t = os.path.join(scratch, f"template-{count}")
    shutil.rmtree(t, ignore_errors=True)
    disks = []
    if count == "join":
        disks = ["--join"]
    for i in range(0 if count == "join" else int(count)):
        empty = os.path.join(scratch, f"empty-{count}-{i}")
        open(empty, "wb").close()
        disks += ["--disk", empty + ":ro"]
    saved = subprocess.run(
        [shards, "run", "--kernel", kernel, "--init", init, "--rootfs", rootfs,
         "--snapshot-dir", t, "--no-console", *disks],
        env=env, stdin=subprocess.DEVNULL, capture_output=True)
    assert saved.returncode == 0, saved.stderr.decode()
    templates[count] = t


def restore(template):
    start = time.perf_counter()
    done = subprocess.run([shards, "restore", template, "--", "/bin/true"], env=env,
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.PIPE)
    wall = (time.perf_counter() - start) * 1e6
    assert done.returncode == 0, done.stderr.decode()
    line = next(l for l in done.stderr.decode().splitlines() if l.startswith("shards-timing "))
    timing = json.loads(line.split(" ", 1)[1])
    return wall, timing["released_us"]


for count in counts:
    restore(templates[count])
samples = {count: [] for count in counts}
for i in range(n):
    order = counts[i % len(counts):] + counts[:i % len(counts)]
    for count in order:
        samples[count].append(restore(templates[count]))


def q(v, f):
    v = sorted(v)
    return v[min(len(v) - 1, int(f * len(v)))]


rng = random.Random(0)
print("load %.2f %.2f %.2f" % os.getloadavg())
base = counts[0]
for part, of in (("wall", lambda w, r: w), ("released", lambda w, r: r)):
    for count in counts:
        v = [of(*s) for s in samples[count]]
        line = f"{part:8} {count:>5} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f} us"
        if count != base:
            d = [of(*b) - of(*a) for a, b in zip(samples[base], samples[count])]
            boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
            line += f"; less {base}: {q(d, .5):+.0f} [{boot[50]:+.0f}, {boot[1949]:+.0f}]"
        print(line)
