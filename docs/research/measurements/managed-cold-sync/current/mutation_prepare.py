#!/usr/bin/env python3
"""Remove only Darwin adapter selection, then export the exact original again."""
import argparse
import base64
import copy
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

REF = "cd0649bfedf2537bb4f9878b6e82c2c6b6e5faf0"
SOURCE = "crates/api/src/sync.rs"


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
    harness = module.git(REF, "docs/research/measurements/managed-cold-sync/current/native_case.rs")
    restored = module.version("restored-cd0649", REF, True, harness)
    mutant = copy.deepcopy(restored)
    mutant["label"] = "std-adapter-mutant"
    mutant["expected_cold_abort"] = True
    original = mutant["files"][SOURCE]["text"]
    apple = '#[cfg(target_vendor = "apple")]\nmod implementation'
    other = '#[cfg(not(target_vendor = "apple"))]\nmod implementation'
    if original.count(apple) != 1 or original.count(other) != 1:
        raise ValueError("adapter selection anchors changed")
    changed = original.replace(apple, "#[cfg(any())]\nmod implementation")
    changed = changed.replace(other, "#[cfg(all())]\nmod implementation")
    if changed.replace("#[cfg(any())]\nmod implementation", apple).replace(
            "#[cfg(all())]\nmod implementation", other) != original:
        raise ValueError("mutation is not exactly reversible")
    body = changed.encode()
    mutant["files"][SOURCE] = {"text": changed, "sha256": hashlib.sha256(body).hexdigest(),
                               "bytes": len(body), "private_test_edits": True}
    versions = [mutant, restored]
    packed = gzip.compress(json.dumps({"versions": versions}, separators=(",", ":")).encode())
    (args.out / "input.b64").write_text(base64.b64encode(packed).decode())
    driver = (here / "binding_run.py").read_bytes()
    (args.out / "driver.b64").write_text(base64.b64encode(driver).decode())
    identities = {"revision": REF, "mutation": "only the two implementation cfg selections",
                  "restored_sync_sha256": hashlib.sha256(original.encode()).hexdigest(),
                  "mutant_sync_sha256": hashlib.sha256(body).hexdigest(),
                  "packed_sha256": hashlib.sha256(packed).hexdigest(),
                  "driver_sha256": hashlib.sha256(driver).hexdigest(),
                  "versions": [{**{k: v for k, v in item.items() if k != "files"},
                                "files": {name: {key: value for key, value in record.items() if key != "text"}
                                          for name, record in item["files"].items()}}
                               for item in versions]}
    (args.out / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
    print(json.dumps({k: v for k, v in identities.items() if k != "versions"}))


if __name__ == "__main__":
    main()
