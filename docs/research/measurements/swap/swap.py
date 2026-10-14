"""What a run's swap costs, and what the swap window costs a microVM (D159,
docs/research/platform-measurements.md M230).

An image of TESTGUEST (crates/testguest, a static guest binary) as /bin/testguest, its
entrypoint, is loaded into a fresh home in SCRATCH, whose daemon the first run starts.
Then each pair of arms, interleaved, their order alternating, N turns:

- start: `run --rm -m 64m IMAGE exit 0` with swap (by default as much again as its
  memory) and without (`--memory-swap 64m`): the daemon's swap file, the VM's mapping of
  it and init's swapon, on the run's path.
- swapping: `run --rm -m 64m IMAGE alloc 96`, 32 MiB past its memory, swapped out,
  against `-m 128m --memory-swap 128m`, where none is.
- boot: `run --kernel KERNEL --init INIT --rootfs ROOT --memory 512 -- /bin/testguest
  exit 0`, ROOT the image's EROFS file in the home, a microVM booted with a swap window as
  large as WINDOW_MIB (default 65536), which init shows and has probed raw, against one
  without.
- walk: the same with `--memory 1024`, `walk 512 STEPS` (default 4000000): a load whose
  translation each step misses, with the window's address space (its IPA 40 bits on
  arm64, where the VM alone needs 36) and without.

Times are the client's, spawn to reap, in µs; for the daemon's runs also SHARDS_TIMING's
request to answer, the VM's own clock; for `walk`, the nanoseconds a step took, as the
guest says. Reported per arm: n, p50, p90, p99, max; per pair, the median of the turns'
paired differences, with a bootstrap 95% interval.

    python3 swap.py SHARDS KERNEL INIT TESTGUEST SCRATCH N
"""
import hashlib, io, json, os, platform, random, shutil, statistics, subprocess, sys, tarfile, time

shards, kernel, init, testguest, scratch, n = sys.argv[1:6] + [int(sys.argv[6])]
window = os.environ.get("WINDOW_MIB", "65536")
steps = os.environ.get("STEPS", "4000000")
home = os.path.join(scratch, "home")
shutil.rmtree(home, ignore_errors=True)
os.makedirs(home)
env = dict(os.environ, SHARDS_HOME=home, SHARDS_TIMING="1")
IMAGE = "swap-bench:1"


def archive(path):
    """An OCI layout of one image: TESTGUEST at /bin/testguest, its entrypoint."""
    arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "amd64"}[platform.machine()]
    blobs = {}

    def put(b):
        d = hashlib.sha256(b).hexdigest()
        blobs[d] = b
        return d

    layer = io.BytesIO()
    with tarfile.open(fileobj=layer, mode="w", format=tarfile.PAX_FORMAT) as t:
        for d in ("bin/",):
            info = tarfile.TarInfo(d)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            t.addfile(info)
        data = open(testguest, "rb").read()
        info = tarfile.TarInfo("bin/testguest")
        info.size = len(data)
        info.mode = 0o755
        t.addfile(info, io.BytesIO(data))
    layer = layer.getvalue()
    layer_digest = put(layer)
    config = json.dumps({
        "architecture": arch, "os": "linux", "created": "2026-10-14T00:00:00Z",
        "config": {"Entrypoint": ["/bin/testguest"]},
        "rootfs": {"type": "layers", "diff_ids": ["sha256:" + layer_digest]},
    }).encode()
    config_digest = put(config)
    manifest = json.dumps({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": "sha256:" + config_digest, "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": "sha256:" + layer_digest, "size": len(layer)}],
    }).encode()
    index = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
                        "manifests": [{
                            "mediaType": "application/vnd.oci.image.manifest.v1+json",
                            "digest": "sha256:" + put(manifest), "size": len(manifest),
                            "annotations": {"io.containerd.image.name": "docker.io/library/" + IMAGE,
                                            "org.opencontainers.image.ref.name": "1"}}]}).encode()
    with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as t:
        def add(name, b):
            info = tarfile.TarInfo(name)
            info.size = len(b)
            t.addfile(info, io.BytesIO(b))
        add("oci-layout", b'{"imageLayoutVersion":"1.0.0"}')
        add("index.json", index)
        for d, b in blobs.items():
            add("blobs/sha256/" + d, b)


def timed(args, expect=None):
    """The client's µs, spawn to reap; SHARDS_TIMING's request to answer, if it says one;
    and its stdout."""
    start = time.perf_counter()
    done = subprocess.run([shards, *args], env=env, stdin=subprocess.DEVNULL, capture_output=True)
    wall = (time.perf_counter() - start) * 1e6
    out, err = done.stdout.decode(), done.stderr.decode()
    assert done.returncode == 0, f"{args}: {done.returncode} {out} {err}"
    if expect is not None:
        assert expect in out, f"{args}: {out!r}"
    line = next((l for l in err.splitlines() if l.startswith("shards-timing ")), None)
    command = None
    if line:
        t = json.loads(line.split(" ", 1)[1])
        command = t["answered_us"] - t["request_us"]
    return wall, command, out


def run(flags, cmd, expect=None):
    return timed(["run", "--rm", "--pull", "never", *flags, IMAGE, *cmd], expect)


def root():
    """The image's EROFS file, which the daemon made as it first ran the image."""
    d = os.path.join(home, "images", "rootfs", "v2")
    found = [f for f in os.listdir(d) if f.endswith(".erofs")]
    assert len(found) == 1, found
    return os.path.join(d, found[0])


def booted(extra, cmd, expect=None):
    return timed(["run", "--kernel", kernel, "--init", init, "--rootfs", root(), "--no-console",
                  *extra, "--", "/bin/testguest", *cmd], expect)


def walked(extra):
    _, _, out = booted(["--memory", "1024", *extra], ["walk", "512", steps], "walk ")
    return float(out.split()[1])


PAIRS = {
    "start": (lambda: run(["-m", "64m", "--memory-swap", "64m"], ["exit", "0"]),
              lambda: run(["-m", "64m"], ["exit", "0"])),
    "swapping": (lambda: run(["-m", "128m", "--memory-swap", "128m"], ["alloc", "96"], "allocated 96"),
                 lambda: run(["-m", "64m"], ["alloc", "96"], "allocated 96")),
    "boot": (lambda: booted(["--memory", "512"], ["exit", "0"]),
             lambda: booted(["--memory", "512", "--swap-window", window], ["exit", "0"])),
    "walk": (lambda: (None, None, walked([])),
             lambda: (None, None, walked(["--swap-window", window]))),
}


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def summary(xs):
    return (f"n={len(xs)} p50={pct(xs, .5):,.1f} p90={pct(xs, .9):,.1f} "
            f"p99={pct(xs, .99):,.1f} max={max(xs):,.1f}")


def paired(a, b):
    d = [y - x for x, y in zip(a, b)]
    boot = sorted(statistics.median(random.choices(d, k=len(d))) for _ in range(2000))
    return f"{statistics.median(d):+,.1f} [{boot[50]:+,.1f}, {boot[1949]:+,.1f}]"


tar = os.path.join(scratch, "swap-bench.tar")
archive(tar)
loaded = subprocess.run([shards, "load", "-q", "-i", tar], env=env, capture_output=True)
assert loaded.returncode == 0, loaded.stderr.decode()
# Its root made, for the microVMs booted directly.
run([], ["exit", "0"])
which = sys.argv[7:] or list(PAIRS)
for name in which:
    without, with_ = PAIRS[name]
    # Each arm once first: the daemon started, its templates saved.
    without()
    with_()
    samples = {"without": [], "with": []}
    for i in range(n):
        order = [("without", without), ("with", with_)]
        if i % 2:
            order.reverse()
        for arm, f in order:
            samples[arm].append(f())
    print(f"== {name}", flush=True)
    for field, label in ((0, "client µs"), (1, "request to answer µs"), (2, "walk ns a step")):
        rows = {arm: [s[field] for s in samples[arm]] for arm in samples}
        if field == 2:
            if name != "walk":
                continue
            rows = {arm: [s[2] for s in samples[arm]] for arm in samples}
        if any(x is None for xs in rows.values() for x in xs):
            continue
        for arm in ("without", "with"):
            print(f"  {label} {arm}: {summary(rows[arm])}")
        print(f"  {label} with - without: {paired(rows['without'], rows['with'])}", flush=True)
