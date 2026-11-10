#!/usr/bin/env python3
"""Pin the current-main closure and exactly three working-tree API edits."""
import argparse
import base64
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(mode=0o700)
    here = Path(__file__).resolve().parent
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("cold_prepare", here / "prepare.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    baseline = "33df95b08ad8f95bddeaa1c48acf0c64386e0eac"
    version = module.version("binding-33df-v1", baseline, True, (here / "native_case.rs").read_bytes())
    registration = (b'\n#[cfg(all(test, target_vendor = "apple"))]\n'
                    b'#[path = "../../../docs/research/measurements/managed-cold-sync/current/native_case.rs"]\n'
                    b'mod cold_sync_current;\n')
    overlays = ["crates/api/src/sync.rs", "crates/api/src/client.rs", "crates/api/src/lib.rs"]
    for path in overlays:
        body = (root / path).read_bytes()
        original_sha = hashlib.sha256(body).hexdigest()
        if path.endswith("client.rs"):
            body += registration
        version["files"][path] = {"text": body.decode(), "sha256": hashlib.sha256(body).hexdigest(),
                                  "bytes": len(body), "working_tree_sha256": original_sha,
                                  "private_test_edits": path.endswith("client.rs")}
    version["working_tree_parent"] = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    packed = gzip.compress(json.dumps({"versions": [version]}, separators=(",", ":")).encode())
    (args.out / "input.b64").write_text(base64.b64encode(packed).decode())
    driver = (here / "binding_run.py").read_bytes()
    (args.out / "driver.b64").write_text(base64.b64encode(driver).decode())
    identities = {"baseline": baseline, "working_tree_parent": version["working_tree_parent"],
                  "overlays": overlays, "packed_bytes": len(packed),
                  "packed_sha256": hashlib.sha256(packed).hexdigest(),
                  "driver_sha256": hashlib.sha256(driver).hexdigest(),
                  "files": {name: {key: value for key, value in record.items() if key != "text"}
                            for name, record in version["files"].items()}}
    (args.out / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
    print(json.dumps({"directory": str(args.out), "packed_bytes": len(packed),
                      "packed_sha256": identities["packed_sha256"],
                      "driver_sha256": identities["driver_sha256"]}))


if __name__ == "__main__":
    main()
