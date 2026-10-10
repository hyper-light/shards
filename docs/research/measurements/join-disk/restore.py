"""What a restore costs a microVM for each empty disk it carries (docs/research/
platform-measurements.md M168, M170): the price of a join disk in every microVM, empty
until a container joins its network, and of a join share, served by none until a joiner
brings volumes (D119).

For each VARIANT, a template is saved from ROOTFS (an image's EROFS file): a count, with
that many empty read-only virtio-blk disks; `join`, with a join disk and none; or
`joinshare`, with a join disk and a join share (virtio-fs); `share`, with a join disk
and a shared directory's device, served by none; then each is restored N
times, the variants interleaved and their order rotating each round:
`shards restore TEMPLATE -- /bin/true`, timed from spawn to reap, and split by the timing
line SHARDS_TIMING adds: `released_us`, the VM process's start to its vCPUs' release (the
restore), and the rest; and the VM process's peak RSS, at the timing line, before it
ends (`rss_kib`, KiB), and over its whole life (wait4's ru_maxrss: bytes on macOS, KiB on
Linux); and its memory as memory per VM is counted, at the timing line (`footprint_kib`:
macOS's phys_footprint, Linux's RssAnon), without the template's pages it shares. Each variant is compared with the first, paired by round: the
median difference with a bootstrap 95% interval. A variant may end in `#NAME`, a template of
its own of the same machine, for what templates of one machine differ by.

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
    machine = count.split("#")[0]
    disks = []
    if machine == "join":
        disks = ["--join"]
    if machine == "joinshare":
        disks = ["--join", "--join-share"]
    if machine == "share":
        disks = ["--join", "--shares", "1"]
    for i in range(0 if machine in ("join", "joinshare", "share") else int(machine)):
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
for part, of in (("wall", lambda w, r, m, k, f: w), ("released", lambda w, r, m, k, f: r),
                 ("maxrss", lambda w, r, m, k, f: m), ("rss_kib", lambda w, r, m, k, f: k),
                 ("footprint_kib", lambda w, r, m, k, f: f)):
    for count in counts:
        v = [of(*s) for s in samples[count]]
        unit = "" if part in ("maxrss", "rss_kib", "footprint_kib") else " us"
        line = f"{part:8} {count:>9} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f}{unit}"
        if count != base:
            d = [of(*b) - of(*a) for a, b in zip(samples[base], samples[count])]
            boot = sorted(q([rng.choice(d) for _ in d], 0.5) for _ in range(2000))
            line += f"; less {base}: {q(d, .5):+.0f} [{boot[50]:+.0f}, {boot[1949]:+.0f}]"
        print(line)
