#!/usr/bin/env python3
"""Builds a real Dockerfile with BuildKit (`docker build`) and with shards, and holds them
together: each layer entry by entry (path, type, mode, owner, size, link, xattrs, SHA-256 of
its bytes, mtime), the image config, shards' root filesystem against its own layers, and
the image run by each, `docker run` and a shards microVM. An mtime may differ only where
each side stamped its own build's wall clock (a directory a step made, say).

    compare.py BIN VERIFY CONTEXT DOCKERFILE NAME [-- CMD...]

BIN is the directory of a release build's `shards`, `shardsd` and `shards-vm`, which are
copied side by side and, on macOS, ad-hoc signed as scripts/hvf-run signs them, so that
`shards-vm` may start VMs; VERIFY is docs/research/measurements/build-memory's
`verify`, built against the revision under test. Prints one JSON line of the result.
"""
import hashlib, io, json, os, shutil, subprocess, sys, tarfile, tempfile, time

bindir, verify, ctx, dockerfile, name = sys.argv[1:6]
cmd = sys.argv[7:] if len(sys.argv) > 6 and sys.argv[6] == "--" else []
work = tempfile.mkdtemp(prefix=f"real-{name}-")
home = os.path.join(work, "home")
os.makedirs(home)
tag = f"shards-real-{name}:1"
resources = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "resources")
bins = os.path.join(work, "bin")
os.makedirs(bins)
for b in ("shards", "shardsd", "shards-vm", "shards-net"):
    shutil.copy2(os.path.join(bindir, b), os.path.join(bins, b))
    if sys.platform == "darwin":
        sign = ["--entitlements", os.path.join(resources, "vm.entitlements"), "-o", "runtime"] if b == "shards-vm" \
            else ["--entitlements", os.path.join(resources, "hvf.entitlements")]
        subprocess.run(["codesign", *sign, "--force", "-s", "-", os.path.join(bins, b)], check=True, capture_output=True)
shards = os.path.join(bins, "shards")


def run(argv, env=None, check=True):
    p = subprocess.run(argv, env=env, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise SystemExit(f"{' '.join(argv)}: {p.returncode}\n{p.stdout}\n{p.stderr}")
    return p


def entries(tar_bytes):
    out = {}
    with tarfile.open(fileobj=io.BytesIO(tar_bytes)) as t:
        for m in t.getmembers():
            data = t.extractfile(m).read() if m.isfile() else b""
            out[m.name.rstrip("/")] = {
                "type": m.type.decode(), "mode": oct(m.mode), "uid": m.uid, "gid": m.gid,
                "size": m.size if m.isfile() else 0, "link": m.linkname,
                "xattrs": {k: v for k, v in m.pax_headers.items() if k.startswith("SCHILY.xattr.")},
                "sha256": hashlib.sha256(data).hexdigest() if m.isfile() else "",
                "mtime": int(m.mtime),
            }
    return out


def decompress(blob):
    if blob[:2] == b"\x1f\x8b":
        import gzip
        return gzip.decompress(blob)
    if blob[:4] == b"\x28\xb5\x2f\xfd":
        return run(["zstd", "-d", "-c"], check=False).stdout.encode()  # not expected
    return blob


def oci_layout_image(root, index_digest=None):
    blob = lambda d: open(os.path.join(root, "blobs", *d.split(":")), "rb").read()
    index = json.load(open(os.path.join(root, "index.json")))
    m = json.loads(blob(index["manifests"][0]["digest"]))
    while "manifests" in m:
        arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "amd64"}[os.uname().machine]
        pick = [x for x in m["manifests"] if x.get("platform", {}).get("architecture") == arch]
        m = json.loads(blob((pick or m["manifests"])[0]["digest"]))
    config = json.loads(blob(m["config"]["digest"]))
    layers = [entries(decompress(blob(l["digest"]))) for l in m["layers"]]
    return config, layers


# BuildKit.
t0 = time.time()
dbuilt = run(["docker", "build", "--no-cache", "--progress=plain", "-f", os.path.join(ctx, dockerfile), "-t", tag, ctx])
t1 = time.time()
save = os.path.join(work, "docker")
os.makedirs(save)
run(["docker", "save", "-o", os.path.join(work, "docker.tar"), tag])
run(["tar", "-xf", os.path.join(work, "docker.tar"), "-C", save])
dconfig, dlayers = oci_layout_image(save)

# shards.
env = dict(os.environ, SHARDS_HOME=home, SHARDS_TIMING="1")
s0 = time.time()
built = run([shards, "build", "--progress=plain", "-f", os.path.join(ctx, dockerfile), "-t", tag, ctx], env=env)
s1 = time.time()
export = next((l.split(" ", 1)[1] for l in built.stderr.splitlines() if l.startswith("shards-export ")), None)
store = os.path.join(home, "images")
blob = lambda d: open(os.path.join(store, "blobs", *d.split(":")), "rb").read()
want = f"docker.io/library/{tag}"
ref = None
for dp, _, fs in os.walk(os.path.join(store, "refs")):
    for f in fs:
        try:
            r = json.load(open(os.path.join(dp, f)))
        except (ValueError, OSError):
            continue
        if r.get("reference") == want:
            ref = r
man = json.loads(blob(ref["manifest"]["digest"]))
sconfig = json.loads(blob(man["config"]["digest"]))
slayers = [entries(decompress(blob(l["digest"]))) for l in man["layers"]]
verified = run([verify, home, f"docker.io/library/{tag}"], check=False).stdout.strip()

# Layers, entry by entry.
def within(t, a, b):
    return a - 2 <= t <= b + 2

diffs = []
if len(dlayers) != len(slayers):
    diffs.append(f"layers: docker {len(dlayers)}, shards {len(slayers)}")
for i, (d, s) in enumerate(zip(dlayers, slayers)):
    for p in sorted(set(d) | set(s)):
        a, b = d.get(p), s.get(p)
        if a is None or b is None:
            diffs.append(f"layer {i} {p}: only in {'shards' if a is None else 'docker'}")
            continue
        for k in a:
            if a[k] == b[k]:
                continue
            if k == "mtime" and within(a[k], t0, t1) and within(b[k], s0, s1):
                continue
            diffs.append(f"layer {i} {p}: {k} docker {a[k]!r} shards {b[k]!r}")

# What each RUN printed, as the plain progress shows it: by step, less the timestamps.
import re
def step_output(text):
    steps, names, out = {}, {}, {}
    for l in text.splitlines():
        m = re.match(r"#(\d+) \[[^\]]*\] (RUN .*)", l)
        if m:
            names[m.group(1)] = m.group(2)
            continue
        m = re.match(r"#(\d+) \d+\.\d+ (.*)", l)
        if m and m.group(1) in names:
            out.setdefault(names[m.group(1)], []).append(m.group(2))
    return out
dout, sout = step_output(dbuilt.stdout + dbuilt.stderr), step_output(built.stdout + built.stderr)
for k in sorted(set(dout) | set(sout)):
    if dout.get(k) != sout.get(k):
        import difflib
        d = list(difflib.unified_diff(dout.get(k, []), sout.get(k, []), "docker", "shards", n=0, lineterm=""))
        diffs.append(f"output of {k[:60]}: " + "\n".join(d))

# Config.
for k in ("Env", "Entrypoint", "Cmd", "WorkingDir", "User", "ExposedPorts", "Labels", "Volumes", "StopSignal", "Shell", "Healthcheck"):
    a, b = dconfig.get("config", {}).get(k), sconfig.get("config", {}).get(k)
    if a != b:
        diffs.append(f"config {k}: docker {a!r} shards {b!r}")
dh = [(h.get("created_by"), h.get("empty_layer", False), h.get("comment")) for h in dconfig.get("history", [])]
sh = [(h.get("created_by"), h.get("empty_layer", False), h.get("comment")) for h in sconfig.get("history", [])]
if dh != sh:
    diffs.append(f"history: docker {dh!r} shards {sh!r}")

# Run each.
ran = {}
if cmd:
    dr = run(["docker", "run", "--rm", tag] + cmd, check=False)
    sr = run([shards, "run", "--pull", "never", "--rm", tag] + cmd, env=env, check=False)
    ran = {"docker": [dr.returncode, dr.stdout], "shards": [sr.returncode, sr.stdout]}
    if (dr.returncode, dr.stdout) != (sr.returncode, sr.stdout):
        import difflib
        lines = list(difflib.unified_diff(dr.stdout.splitlines(), sr.stdout.splitlines(), "docker", "shards", n=0, lineterm=""))
        diffs.append(f"run: exit docker {dr.returncode} shards {sr.returncode}; " + "\n".join(lines))

# Serve: SERVE=N runs the image's own command in the background for N seconds under each,
# then compares their logs, each line from its level marker on (times, PIDs and generated
# IDs differ by nature), less the lines naming generated IDs.
serve = float(os.environ.get("SERVE", "0"))
if serve:
    import re
    def norm(text):
        out = []
        for l in text.splitlines():
            m = re.search(r"\[(INF|WRN|ERR|FTL|DBG|TRC)\].*", l)
            l = m.group(0) if m else l
            if re.search(r"(ID|Name|name):\s+\S{20,}|^\[INF\]\s+\S{20,}$", l):
                continue
            out.append(l)
        return out
    dc = run(["docker", "run", "-d", tag]).stdout.strip()
    sc = run([shards, "run", "--pull", "never", "-d", tag], env=env).stdout.strip()
    time.sleep(serve)
    dl = run(["docker", "logs", dc], check=False)
    sl = run([shards, "logs", sc], env=env, check=False)
    dps = run(["docker", "inspect", "-f", "{{.State.Running}}", dc], check=False).stdout.strip()
    sps = run([shards, "ps", "-q", "--no-trunc"], env=env, check=False).stdout.split()
    ran["serve"] = {"docker": norm(dl.stdout + dl.stderr), "shards": norm(sl.stdout + sl.stderr),
                    "running": {"docker": dps == "true", "shards": any(sc.startswith(x) or x.startswith(sc) for x in sps)}}
    if ran["serve"]["docker"] != ran["serve"]["shards"] or not all(ran["serve"]["running"].values()):
        diffs.append(f"serve: {ran['serve']!r}")
    run(["docker", "rm", "-f", dc], check=False)
    run([shards, "rm", "-f", sc], env=env, check=False)
    run([shards, "daemon", "stop"], env=env, check=False)

run(["docker", "rmi", "-f", tag], check=False)
shutil.rmtree(work, ignore_errors=True)
print(json.dumps({"name": name, "export": export, "rootfs": verified, "diffs": diffs, "ran": ran,
                  "seconds": {"docker": round(t1 - t0, 2), "shards": round(s1 - s0, 2)}}))
