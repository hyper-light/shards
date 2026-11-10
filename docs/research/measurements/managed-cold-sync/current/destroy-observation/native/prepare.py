#!/usr/bin/env python3
"""Snapshot the reviewed four-crate closure and its finite ordinary supervisor."""
import argparse
import base64
import gzip
import hashlib
import json
from pathlib import Path
import posixpath
import re
import subprocess
import sys
import uuid

SOURCE = "79276d1923811bde0b275dda7afa98f587737f3f"
POLICY = "0342b85d9c808e3ced041a6c3ef0e9bf70438272"
RULES = "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"


def blob(ref, name):
    return subprocess.check_output(["git", "show", ref + ":" + name])


def record(body):
    return {"text": body.decode(), "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(mode=0o700)
    here = Path(__file__).resolve().parent
    sys.dont_write_bytecode = True
    original = blob(SOURCE, "docs/research/measurements/managed-cold-sync/current/prepare.py")
    namespace = {"__name__": "frozen_closure", "__file__": str(here.parents[1] / "prepare.py")}
    exec(compile(original, namespace["__file__"], "exec"), namespace)
    version = namespace["version"]("ordinary-final-owner-792", SOURCE, True, b"")
    # Undo the old exporter's private cold-refusal registration, then use the
    # complete reviewed client verbatim. No old cold actor is included or run.
    version["files"]["crates/api/src/client.rs"] = record(blob(SOURCE, "crates/api/src/client.rs"))
    del version["files"]["docs/research/measurements/managed-cold-sync/current/native_case.rs"]
    version["has_allocation_refusal"] = False
    for leaf in ["repro.rs", "reader-tests.rs"]:
        name = "docs/research/measurements/managed-proc-main/" + leaf
        version["files"][name] = record(blob(SOURCE, name))
    pending = [name for name in version["files"] if name.endswith(".rs")]
    seen = set()
    while pending:
        name = pending.pop()
        if name in seen:
            continue
        seen.add(name)
        for relative in re.findall(r'#\[path\s*=\s*"([^"]+)"\]', version["files"][name]["text"]):
            target = posixpath.normpath(posixpath.join(posixpath.dirname(name), relative))
            if target.startswith("../") or target.startswith("/"):
                raise ValueError("source import escapes repository")
            if target not in version["files"]:
                version["files"][target] = record(blob(SOURCE, target))
                pending.append(target)
    version["files"][".cargo/config.toml"] = record(blob(SOURCE, ".cargo/config.toml"))
    for name in ["AGENTS.md", "CLAUDE.md", "docs/development-rules.md", "docs/development-evidence.md",
                 "docs/design/managed-runtime-plan.md"]:
        version["files"][name] = record(blob(POLICY, name))
    version["files"]["policy/g121-handoff.md"] = record(Path("/Users/adalundhe/g121-handoff.md").read_bytes())
    for name, value in version["files"].items():
        if name.startswith("crates/") or name.endswith(".rs"):
            if value["text"].encode() != blob(SOURCE, name):
                raise ValueError("source differs from reviewed commit: " + name)
    if version["files"]["docs/development-rules.md"]["sha256"] != RULES:
        raise ValueError("policy digest mismatch")
    helper = blob(SOURCE, "docs/research/measurements/managed-cold-sync/current/binding_run.py")
    prefix = helper.split(b"\ndef ordinary(", 1)[0]
    old = b'"argv": [str(binary), selector, "--exact", "--test-threads=1", "--nocapture"],'
    new = b'"argv": [str(binary), selector, "--exact", "--ignored", "--test-threads=1", "--nocapture"],'
    if prefix.count(old) != 1:
        raise ValueError("exact held-supervisor argv anchor changed")
    held = prefix.replace(old, new)
    if held.replace(new, old) != prefix:
        raise ValueError("supervisor changed beyond the ignored selector flag")
    with (here / "held_exit.py").open("xb") as output:
        output.write(held)
    remote = "/Users/adalundhe/managed-cold-sync-destroy-792-v1-" + uuid.uuid4().hex
    bundle = {"version": version, "source_revision": SOURCE, "policy_revision": POLICY,
              "rules_sha256": RULES, "remote_root": remote,
              "driver": record((here / "run.py").read_bytes()), "helper": record(held)}
    packed = gzip.compress(json.dumps(bundle, separators=(",", ":")).encode(), mtime=0)
    (args.out / "input.json.gz.b64").write_text(base64.b64encode(packed).decode() + "\n")
    identities = {"source_revision": SOURCE, "policy_revision": POLICY, "rules_sha256": RULES,
                  "remote_root": remote, "packed_sha256": hashlib.sha256(packed).hexdigest(),
                  "packed_bytes": len(packed), "files": {name: {k: v for k, v in value.items() if k != "text"}
                  for name, value in version["files"].items()},
                  "driver": {k: v for k, v in bundle["driver"].items() if k != "text"},
                  "helper": {k: v for k, v in bundle["helper"].items() if k != "text"},
                  "original_supervisor_sha256": hashlib.sha256(helper).hexdigest(),
                  "copied_supervisor_prefix_sha256": hashlib.sha256(prefix).hexdigest(),
                  "helper_delta": "Only --ignored in case argv; unused old ordinary/main omitted; same actual case function/observation/deadline/cleanup",
                  "resource_envelope": {"cpus": 2, "mem_gib": 2, "disk_gib": 4, "jobs": 2,
                                        "compile_seconds": 180, "case_seconds": 15, "outer_seconds": 600},
                  "no_allocation_refusal": True, "native_cases_started": 0}
    (args.out / "snapshot.json").write_text(json.dumps(identities, indent=2) + "\n")
    print(json.dumps({"snapshot": str(args.out / "snapshot.json"), "remote_root": remote,
                      "packed_bytes": len(packed), "packed_sha256": identities["packed_sha256"],
                      "driver": identities["driver"], "helper": identities["helper"]}))


if __name__ == "__main__":
    main()
