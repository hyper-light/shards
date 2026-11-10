#!/usr/bin/env python3
"""Export exact f08 API bytes; reuse the complete frozen four-crate exporter."""
import argparse
import base64
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

REF = "f08cc06b009158a5eafce00d3bc806b702a8aff7"


def sha(body):
    return hashlib.sha256(body).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(mode=0o700)
    here = Path(__file__).resolve().parent
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("frozen_export", here.parent / "linux/prepare.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.REF = REF
    previous = sys.argv
    try:
        sys.argv = ["prepare.py", "--out", str(args.out / "frozen-export")]
        helper.main()
    finally:
        sys.argv = previous
    original = base64.b64decode((args.out / "frozen-export/input.b64").read_text(), validate=True)
    version = json.loads(gzip.decompress(original))["version"]
    assert version["revision"] == REF and not version["has_allocation_refusal"]
    version["label"] = "windows-f08-ordinary"
    for name, record in version["files"].items():
        if name.startswith("crates/") or name.endswith(".rs"):
            assert record["text"].encode() == subprocess.check_output(["git", "show", REF + ":" + name])
    packed = gzip.compress(json.dumps({"version": version}, separators=(",", ":")).encode())
    driver = (here / "run.ps1").read_bytes()
    (args.out / "input.b64").write_text(base64.b64encode(packed).decode())
    (args.out / "driver.b64").write_text(base64.b64encode(driver).decode())
    record = {"revision": REF, "policy_revision": helper.POLICY,
              "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
              "packed_sha256": sha(packed), "driver_sha256": sha(driver),
              "files": {name: {k: v for k, v in value.items() if k != "text"}
                        for name, value in version["files"].items()}}
    (args.out / "identities.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps({k: v for k, v in record.items() if k != "files"}))


if __name__ == "__main__":
    main()
