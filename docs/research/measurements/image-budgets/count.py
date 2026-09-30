"""Entries, uncompressed bytes, and bytes of names, links and xattrs of every image a
shards store has recorded (PM M47): the sizes A10's default limits are set against.

    python3 count.py SHARDS_HOME/images
"""
import json, os, sys, tarfile

root = sys.argv[1]
blob = lambda d: os.path.join(root, "blobs", *d.split(":"))
for rec in sorted(os.listdir(os.path.join(root, "refs/v1"))):
    tag = json.load(open(os.path.join(root, "refs/v1", rec)))
    manifest = json.load(open(blob(tag["manifest"]["digest"])))
    entries = size = meta = 0
    for layer in manifest["layers"]:
        with tarfile.open(blob(layer["digest"]), "r:*") as t:
            for e in t:
                entries += 1
                size += e.size + 512
                meta += len(e.name.encode()) + len(e.linkname.encode())
                meta += sum(len(k) + len(v) for k, v in e.pax_headers.items() if k.startswith("SCHILY.xattr."))
    print(f"{tag['reference']:40} layers {len(manifest['layers']):2} entries {entries:7} "
          f"uncompressed {size / 2**30:5.2f} GiB metadata {meta / 2**20:6.1f} MiB")
