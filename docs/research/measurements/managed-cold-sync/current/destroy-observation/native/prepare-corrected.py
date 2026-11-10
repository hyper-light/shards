#!/usr/bin/env python3
"""Prepare the exact Debug correction packet; do not execute or admit a run."""
import base64
import gzip
import hashlib
import json
from pathlib import Path
import subprocess
import uuid

OLD = "79276d1923811bde0b275dda7afa98f587737f3f"
NEW = "10f282bc642f37be54c701bd4ce389574b641e1d"
OBSERVER = "docs/research/measurements/managed-cold-sync/current/destroy-observation/observer.rs"


def record(body):
    return {"text": body.decode(), "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()}


def main():
    here = Path(__file__).resolve().parent
    packed = base64.b64decode((here / "inputs-v1/input.json.gz.b64").read_text().strip(), validate=True)
    if hashlib.sha256(packed).hexdigest() != "fa8e3dfa0f2a5463029f5ead19b79d3e8be96ddb0d039440d8c7e2732a8a6bd3":
        raise ValueError("original signed input mismatch")
    bundle = json.loads(gzip.decompress(packed))
    old = bundle["version"]["files"][OBSERVER]["text"].encode()
    fixed = subprocess.check_output(["git", "show", NEW + ":" + OBSERVER])
    if fixed != old.replace(b"#[derive(Default)]", b"#[derive(Default, Debug)]", 1):
        raise ValueError("correction exceeds actual E0277 derive requirement")
    bundle["version"]["files"][OBSERVER] = record(fixed)
    for name, item in bundle["version"]["files"].items():
        if name.startswith("crates/") or name.endswith(".rs"):
            actual = subprocess.check_output(["git", "show", NEW + ":" + name])
            if actual != item["text"].encode():
                raise ValueError("other corrected source changed: " + name)
    old_driver = bundle["driver"]["text"].encode()
    anchor = ('SOURCE = "' + OLD + '"').encode()
    if old_driver.count(anchor) != 1:
        raise ValueError("frozen driver source-pin anchor changed")
    driver = old_driver.replace(anchor, ('SOURCE = "' + NEW + '"').encode(), 1)
    bundle["driver"] = record(driver)
    bundle["source_revision"] = NEW
    bundle["version"]["revision"] = NEW
    bundle["version"]["label"] = "ordinary-final-owner-10f"
    bundle["remote_root"] = "/Users/adalundhe/managed-cold-sync-destroy-10f-v1-" + uuid.uuid4().hex
    output = here / "inputs-corrected-v1"
    output.mkdir(mode=0o700)
    packed = gzip.compress(json.dumps(bundle, separators=(",", ":")).encode(), mtime=0)
    (output / "input.json.gz.b64").write_text(base64.b64encode(packed).decode() + "\n")
    snapshot = {"source_revision": NEW, "prior_source_revision": OLD,
                "policy_revision": bundle["policy_revision"], "rules_sha256": bundle["rules_sha256"],
                "remote_root": bundle["remote_root"], "packed_bytes": len(packed),
                "packed_sha256": hashlib.sha256(packed).hexdigest(),
                "files": {name: {k: v for k, v in item.items() if k != "text"}
                          for name, item in bundle["version"]["files"].items()},
                "driver": {k: v for k, v in bundle["driver"].items() if k != "text"},
                "helper": {k: v for k, v in bundle["helper"].items() if k != "text"},
                "only_rust_delta": "SharedWitness derives Debug after actual E0277",
                "only_driver_delta": "exact corrected-source constant; no algorithm/deadline change",
                "native_case_attempts": 0, "further_execution_held_for_root_review": True}
    (output / "snapshot.json").write_text(json.dumps(snapshot, indent=2) + "\n")
    print(json.dumps({k: v for k, v in snapshot.items() if k != "files"}))


if __name__ == "__main__":
    main()
