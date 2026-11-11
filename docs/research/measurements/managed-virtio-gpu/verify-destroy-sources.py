#!/usr/bin/env python3
"""Verify frozen research inputs; does not compile or execute renderer code."""

import hashlib
import json
from pathlib import Path


def check(path, size, digest):
    data = path.read_bytes()
    if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
        raise SystemExit(f"source mismatch: {path}")


base = Path(__file__).resolve().parent
repo = base.parents[3]
sources = json.loads((base / "destroy-source-inputs.json").read_text())
total = 0
archives = 0
for backend in sources:
    for member in backend["files"]:
        check(base / "sources/destroy" / backend["name"] / member["path"],
              member["bytes"], member["sha256"])
        total += 1
    archive = repo / backend["frozen_archive"]
    if archive.exists():
        check(archive, backend["bytes"], backend["expected_sha256"])
        archives += 1
    else:
        print(f"archive unavailable locally (retained source checked): {archive}")
for comparison in json.loads((base / "destroy-comparison-inputs.json").read_text()):
    if comparison.get("status") == 200:
        check(base / "sources/destroy" / (comparison["name"] + ".txt"),
              comparison["bytes"], comparison["sha256"])
        total += 1
selected = base / "destroy-selected-patch-inputs.json"
selected_count = 0
if selected.exists():
    for member in json.loads(selected.read_text())["files"]:
        check(repo / member["retained_path"], member["bytes"], member["sha256"])
        selected_count += 1
print(json.dumps({"source_payloads_checked": total,
                  "original_archives_checked": archives,
                  "supplied_patch_adapter_payloads_checked": selected_count,
                  "renderer_execution": False}, sort_keys=True))
