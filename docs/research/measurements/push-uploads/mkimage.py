#!/usr/bin/env python3
"""An image of fresh random layers, put in a registry for learned.sh (PM M488, D226).

    mkimage.py URL REPO TAG --layers N --size BYTES

Each layer is an uncompressed tar of one file of BYTES random bytes, so no two runs share
a blob; with its config and an OCI manifest, uploaded as push.py uploads (POST, one PUT)
and tagged TAG. Anonymous registries only. Prints the image's reference.
"""
import argparse
import hashlib
import io
import json
import os
import sys
import tarfile
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import push  # noqa: E402


def tar_of(data):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=tarfile.PAX_FORMAT) as t:
        info = tarfile.TarInfo("data")
        info.size = len(data)
        info.mode = 0o644
        t.addfile(info, io.BytesIO(data))
    return out.getvalue()


def digest(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("repo")
    ap.add_argument("tag")
    ap.add_argument("--layers", type=int, required=True)
    ap.add_argument("--size", type=int, required=True)
    a = ap.parse_args()
    base = a.url.rstrip("/")
    layers = [tar_of(os.urandom(a.size)) for _ in range(a.layers)]
    arch = {"x86_64": "amd64", "amd64": "amd64"}.get(os.uname().machine, "arm64")
    config = json.dumps(
        {
            "architecture": arch,
            "os": "linux",
            "config": {"Cmd": ["/bin/true"]},
            "rootfs": {"type": "layers", "diff_ids": [digest(l) for l in layers]},
        }
    ).encode()
    conn = push.connect(base)
    for blob in [config] + layers:
        push.upload(conn, base, a.repo, blob, digest(blob))
    manifest = json.dumps(
        {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": digest(config),
                "size": len(config),
            },
            "layers": [
                {"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest(l), "size": len(l)}
                for l in layers
            ],
        }
    ).encode()
    path = urllib.parse.urlsplit(base).path.rstrip("/")
    conn.request(
        "PUT",
        f"{path}/v2/{a.repo}/manifests/{a.tag}",
        body=manifest,
        headers={"Content-Type": "application/vnd.oci.image.manifest.v1+json"},
    )
    r = conn.getresponse()
    r.read()
    if r.status not in (200, 201):
        raise SystemExit(f"manifest PUT {r.status} {r.reason}")
    host = urllib.parse.urlsplit(base).netloc
    print(f"{host}/{a.repo}:{a.tag}")


if __name__ == "__main__":
    sys.exit(main())
