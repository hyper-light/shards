#!/usr/bin/env python3
"""How long `shards image inspect NAME` takes in a store of many images, for two builds,
interleaved (PM M87). Each build gets a home of its own, into which it loads an OCI
archive of IMAGES distinct images (one shared layer; a config each), so no client meets
a daemon of the other build. The first run of each is not counted: it starts the daemon.

    run.py A_BIN_DIR B_BIN_DIR IMAGES RUNS SCRATCH_DIR

A_BIN_DIR and B_BIN_DIR each hold a release `shards` and `shardsd`.
"""
import hashlib
import io
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tarfile
import time


def archive(path, images):
    arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "amd64", "AMD64": "amd64"}[platform.machine()]
    blobs = {}

    def put(b):
        d = hashlib.sha256(b).hexdigest()
        blobs[d] = b
        return d

    layer = io.BytesIO()
    with tarfile.open(fileobj=layer, mode="w", format=tarfile.USTAR_FORMAT) as t:
        data = b"hello\n"
        info = tarfile.TarInfo("hello.txt")
        info.size = len(data)
        t.addfile(info, io.BytesIO(data))
    layer = layer.getvalue()
    layer_digest = put(layer)
    manifests = []
    for i in range(images):
        config = json.dumps({
            "architecture": arch, "os": "linux", "created": "2026-10-03T00:00:00Z",
            "config": {"Labels": {"n": str(i)}},
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
        manifests.append({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": "sha256:" + put(manifest), "size": len(manifest),
            "annotations": {"io.containerd.image.name": f"docker.io/library/img-{i}:latest",
                            "org.opencontainers.image.ref.name": "latest"},
        })
    index = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
                        "manifests": manifests}).encode()
    with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as t:
        def add(name, b):
            info = tarfile.TarInfo(name)
            info.size = len(b)
            t.addfile(info, io.BytesIO(b))
        add("oci-layout", b'{"imageLayoutVersion":"1.0.0"}')
        add("index.json", index)
        for d, b in blobs.items():
            add("blobs/sha256/" + d, b)


def pct(xs, p):
    xs = sorted(xs)
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main():
    a, b, images, runs, scratch = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
    os.makedirs(scratch, exist_ok=True)
    tar = os.path.join(scratch, "images.tar")
    archive(tar, images)
    homes = {}
    for label, bindir in (("A", a), ("B", b)):
        home = os.path.join(scratch, "home-" + label)
        shutil.rmtree(home, ignore_errors=True)
        os.makedirs(home)
        env = dict(os.environ, SHARDS_HOME=home)
        subprocess.run([os.path.join(bindir, "shards"), "load", "-q", "-i", tar], env=env,
                       check=True, capture_output=True)
        homes[label] = (bindir, env)
    name = f"img-{images // 2}"
    times = {"A": [], "B": []}

    def once(label):
        bindir, env = homes[label]
        start = time.perf_counter()
        subprocess.run([os.path.join(bindir, "shards"), "image", "inspect", name], env=env,
                       check=True, capture_output=True)
        return (time.perf_counter() - start) * 1e3

    once("A")
    once("B")
    for i in range(runs):
        for label in (("A", "B") if i % 2 == 0 else ("B", "A")):
            times[label].append(once(label))
    for label in ("A", "B"):
        xs = times[label]
        print(f"{label}: n={len(xs)} p50={pct(xs, .5):.2f} ms p90={pct(xs, .9):.2f} "
              f"p99={pct(xs, .99):.2f} max={max(xs):.2f} mean={statistics.mean(xs):.2f}")
    for label in ("A", "B"):
        bindir, env = homes[label]
        subprocess.run([os.path.join(bindir, "shards"), "daemon", "stop"], env=env, capture_output=True)


main()
