#!/usr/bin/env python3
"""Ordinary concurrency fixture on frozen cd0649 API bytes; no allocator hook."""
import argparse
import base64
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import sys

REF = "727471d9fe40aaae23dd77c4928ea2aa9b12eb05"
CASE = "docs/research/measurements/managed-cold-sync/current/concurrent_case.rs"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(mode=0o700)
    here = Path(__file__).resolve().parent
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("cold_prepare", here / "prepare.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    version = module.version("ordinary-concurrent-cd0649", REF, True, b"")
    old = ('\n#[cfg(all(test, target_vendor = "apple"))]\n'
           '#[path = "../../../docs/research/measurements/managed-cold-sync/current/native_case.rs"]\n'
           'mod cold_sync_current;\n')
    client = version["files"]["crates/api/src/client.rs"]["text"]
    if not client.endswith(old):
        raise ValueError("private registration anchor changed")
    original = client[:-len(old)]
    registration = ('\n#[cfg(all(test, target_vendor = "apple"))]\n'
                    '#[path = "../../../docs/research/measurements/managed-cold-sync/current/concurrent_case.rs"]\n'
                    'mod concurrent_sync;\n')
    body = (original + registration).encode()
    version["files"]["crates/api/src/client.rs"] = {
        "text": body.decode(), "sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body),
        "production_sha256": hashlib.sha256(original.encode()).hexdigest(), "private_test_edits": True}
    del version["files"]["docs/research/measurements/managed-cold-sync/current/native_case.rs"]
    body = (here / "concurrent_case.rs").read_bytes()
    version["files"][CASE] = {"text": body.decode(), "sha256": hashlib.sha256(body).hexdigest(),
                             "bytes": len(body), "private_test_edits": True}
    version["has_allocation_refusal"] = False
    packed = gzip.compress(json.dumps({"versions": [version]}, separators=(",", ":")).encode())
    (args.out / "input.b64").write_text(base64.b64encode(packed).decode())
    identities = {"revision": REF, "production_revision": "cd0649bfedf2537bb4f9878b6e82c2c6b6e5faf0",
                  "policy_revision": "a920c85b5c0c9d40aafc4f4cce32432c7c0aed79",
                  "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
                  "packed_sha256": hashlib.sha256(packed).hexdigest(), "no_allocator_hook": True,
                  "files": {name: {k: v for k, v in record.items() if k != "text"}
                            for name, record in version["files"].items()}}
    for name in ["concurrent_run", "binding_run"]:
        body = (here / (name + ".py")).read_bytes()
        (args.out / (name + ".b64")).write_text(base64.b64encode(body).decode())
        identities[name + "_sha256"] = hashlib.sha256(body).hexdigest()
    (args.out / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
    print(json.dumps({k: v for k, v in identities.items() if k != "files"}))


if __name__ == "__main__":
    main()
