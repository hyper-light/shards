#!/usr/bin/env python3.12
"""Record the one exact dispatch after all real compilation/list gates, no actor."""
from pathlib import Path
import hashlib
import json
import os
import tarfile
import time
import tomllib

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-v1")
budget = json.loads((root / "budget.json").read_text())
ready = json.loads((root / "build-records/ready.json").read_text())
assert time.time() + 45 < budget["deadline_epoch"]
assert ready["selected_count"] == 1 and ready["native_cases_run"] == 0
assert ready["filter"] == "managed_audit_wire_adversarial::reserved_client_disconnect_retains_owned_extent"
assert hashlib.sha256(Path(ready["executable"]).read_bytes()).hexdigest() == ready["sha256"]
listed = (root / "build-records/list.stdout").read_text()
assert [line for line in listed.splitlines() if line.endswith(": test")] == [ready["filter"] + ": test"]
stages = {}
for name in ["toolchain", "policy", "build", "list", "clippy"]:
    data = json.loads((root / ("build-records/" + name + ".result.json")).read_text())
    assert data["exit_code"] == 0 and not data["timeout"]
    stages[name] = data
usage = os.statvfs(root)
assert usage.f_bavail * usage.f_frsize >= 30 * 1024**3
leaf = root / "source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"
assert hashlib.sha256(leaf.read_bytes()).hexdigest() == "65c73f963edef4ffb81754cf2eea2bd4ecd6a44d7f14178994725b211d0a4c65"
assert hashlib.sha256((root / "source-package.tar.gz").read_bytes()).hexdigest() == "ba43fd22a2036de11268aa4ae49e2b9ce748b5bee7bbd3fcb615a86237b86b63"
identity = lambda row: (row["name"], row["version"], row.get("source"), row.get("checksum"))
with tarfile.open(root / "source-package.tar.gz") as archive:
    original_lock = archive.extractfile("source/Cargo.lock").read().decode()
original = {identity(row) for row in tomllib.loads(original_lock)["package"]}
actual = {identity(row) for row in tomllib.loads((root / "build-records/actual-Cargo.lock").read_text())["package"] if "source" in row}
assert actual <= original
assert not (root / "native-records-v1").exists()
(root / "native-tmp-v1").mkdir()
controller = root / "case-controller-v1"
controller.mkdir()
receipt = {"status": "DispatchRecorded_BeforeSingleCase", "at_epoch": time.time(), "budget": budget,
           "ready": ready, "stages": stages, "free_bytes": usage.f_bavail * usage.f_frsize,
           "source_revision": budget["source_revision"],
           "package_sha256": hashlib.sha256((root / "source-package.tar.gz").read_bytes()).hexdigest(),
           "leaf_sha256": hashlib.sha256(leaf.read_bytes()).hexdigest(),
           "lib_include_sha256": hashlib.sha256((root / "source/crates/vmm/src/lib.rs").read_bytes()).hexdigest(),
           "external_package_identities": sorted(actual),
           "case_native_count": 0, "runtime_retry_allowed": False, "parent_hard_seconds": 45,
           "work_seconds": 30, "cleanup_allowance_seconds": 15}
with (controller / "dispatch-before-case.json").open("x") as file:
    json.dump(receipt, file, indent=2)
print(json.dumps(receipt), flush=True)
