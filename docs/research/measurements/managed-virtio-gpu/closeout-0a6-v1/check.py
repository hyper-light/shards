#!/usr/bin/env python3
"""Check this frozen package; does not build or run a GPU, VM or test actor."""

import hashlib
import json
import shutil
import subprocess
from pathlib import Path


def check(path, size, digest):
    data = path.read_bytes()
    if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
        raise SystemExit(f"input mismatch: {path}")


package = Path(__file__).resolve().parent
evidence = package.parent
repo = evidence.parents[3]
logs = package / "validation"
logs.mkdir(exist_ok=False)
free = shutil.disk_usage(repo).free
if free < 30 * 1024**3:
    raise SystemExit(f"Mac free-space floor not met: {free}")
manifest = json.loads((package / "source-manifest.json").read_text())
for source in manifest["owned_sources"]:
    value = source["after"]
    check(repo / source["path"], value["bytes"], value["sha256"])
    check(evidence / source["retained_source"], value["bytes"], value["sha256"])
patch = manifest["patch"]
check(evidence / patch["path"], patch["bytes"], patch["sha256"])
raw_count = 0
for record in json.loads((package / "native-evidence.json").read_text())["records"]:
    for item in record["retained_files"]:
        check(evidence / item["path"], item["bytes"], item["sha256"])
        raw_count += 1
results = []
commands = [
    ("policy", ["scripts/check-development-policy"]),
    ("format", ["rustfmt", "--check", "--edition", "2024",
                "crates/vmm/src/devices/virtio/gpu/mod.rs",
                "crates/vmm/src/devices/virtio/gpu/protocol.rs"]),
]
for name, argv in commands:
    result = subprocess.run(argv, cwd=repo, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=570, check=False)
    (logs / (name + ".stdout")).write_bytes(result.stdout)
    (logs / (name + ".stderr")).write_bytes(result.stderr)
    results.append({"name": name, "argv": argv, "exit": result.returncode})
receipt = {"source_manifest": "source-manifest.json", "raw_payloads_checked": raw_count,
           "free_bytes_before": free, "free_bytes_after": shutil.disk_usage(repo).free,
           "admission": "Kiln 2CPU/2GiB/4GiB/600s; Mac lockf; no Cargo build/test",
           "results": results, "native_tests_started": False, "renderer_executed": False}
(package / "validation.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps(receipt, sort_keys=True))
raise SystemExit(0 if all(item["exit"] == 0 for item in results) else 1)
