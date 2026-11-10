#!/usr/bin/env python3
"""Export a named four-crate Git closure without a clone or shared target."""
import argparse
import base64
import gzip
import hashlib
import json
from pathlib import Path
import re
import subprocess


def git(ref, path):
    return subprocess.check_output(["git", "show", ref + ":" + path])


def version(label, ref, candidate, harness):
    paths = subprocess.check_output(["git", "ls-tree", "-r", "--name-only", ref, "--",
                                   "crates/api", "crates/abi", "crates/afunix", "crates/testdir"], text=True).splitlines()
    if candidate:
        paths += subprocess.check_output(["git", "ls-tree", "-r", "--name-only", ref, "--",
                                         "docs/research/measurements/managed-borrowed-nowait"], text=True).splitlines()
        paths = [path for path in paths if not path.startswith("docs/") or path.endswith(".rs")]
    paths += ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "clippy.toml"]
    files = {path: git(ref, path) for path in paths}
    root = files["Cargo.toml"].decode()
    root = ('[workspace]\nresolver = "3"\nmembers = ["crates/api", "crates/abi", "crates/afunix", "crates/testdir"]\n\n'
            + root[root.index("[workspace.package]"):])
    # The TLS/QuickJS patches have no dependency in this API closure. Their absent
    # directories must not cause cargo to inspect unrelated repository contents.
    files["Cargo.toml"] = re.sub(r"\[patch\.crates-io\]\n.*?(?=\n\[)", "", root, flags=re.S).encode()
    prefix, *parts = files["Cargo.lock"].decode().split("[[package]]")
    by_name = {}
    entries = []
    for block in parts:
        name = re.search(r'^name = "([^"]+)"', block, re.M).group(1)
        number = re.search(r'^version = "([^"]+)"', block, re.M).group(1)
        deps = re.search(r"^dependencies = \[(.*?)^\]", block, re.M | re.S)
        deps = re.findall(r'"([^"]+)"', deps.group(1)) if deps else []
        item = (name, number, block, deps)
        entries.append(item)
        by_name.setdefault(name, []).append(item)
    needed = set()
    pending = [(name, None) for name in ["shards-api", "shards-abi", "shards-afunix", "shards-testdir"]]
    while pending:
        name, number = pending.pop()
        choices = [item for item in by_name[name] if number is None or item[1] == number]
        if len(choices) != 1:
            raise ValueError("ambiguous lock dependency: " + name)
        name, number, block, deps = choices[0]
        if (name, number) in needed:
            continue
        needed.add((name, number))
        for dependency in deps:
            words = dependency.split(" ")
            pending.append((words[0], words[1] if len(words) > 1 else None))
    files["Cargo.lock"] = (prefix + "".join("[[package]]" + block for name, number, block, _ in entries
                                         if (name, number) in needed)).encode()
    files["crates/api/src/client.rs"] += (b'\n#[cfg(all(test, target_vendor = "apple"))]\n'
        b'#[path = "../../../docs/research/measurements/managed-cold-sync/current/native_case.rs"]\nmod cold_sync_current;\n')
    files["docs/research/measurements/managed-cold-sync/current/native_case.rs"] = harness
    return {"label": label, "revision": ref, "has_allocation_refusal": candidate,
            "files": {path: {"text": body.decode(), "sha256": hashlib.sha256(body).hexdigest(),
                             "bytes": len(body), "private_test_edits": path in ["Cargo.toml", "Cargo.lock", "crates/api/src/client.rs"]}
                      for path, body in files.items()}}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(mode=0o700)
    here = Path(__file__).resolve().parent
    versions = [version("current-cbd392", "cbd39284937d024640c56db6f8d8ef2bd9b0ee24", False, (here / "native_case.rs").read_bytes()),
                version("candidate-d086", "d086894864cd5b3152fbf17b355186f99824155b", True, (here / "native_case.rs").read_bytes())]
    packed = gzip.compress(json.dumps({"versions": versions}, separators=(",", ":")).encode())
    payload = {"packed_b64": base64.b64encode(packed).decode(), "packed_sha256": hashlib.sha256(packed).hexdigest(),
               "packed_bytes": len(packed), "driver_b64": base64.b64encode((here / "run.py").read_bytes()).decode(),
               "driver_sha256": hashlib.sha256((here / "run.py").read_bytes()).hexdigest()}
    with (args.out / "transfer.json").open("x") as stream:
        json.dump(payload, stream)
    identities = [{**{k: v for k, v in item.items() if k != "files"},
                   "files": {path: {k: v for k, v in rec.items() if k != "text"} for path, rec in item["files"].items()}}
                  for item in versions]
    with (args.out / "identities.json").open("x") as stream:
        json.dump(identities, stream, indent=2)
    print(json.dumps({"directory": str(args.out), "transfer_bytes": (args.out / "transfer.json").stat().st_size,
                      "packed_bytes": len(packed), "packed_sha256": payload["packed_sha256"]}))


if __name__ == "__main__":
    main()
