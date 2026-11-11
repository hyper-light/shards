#!/usr/bin/env python3
"""Called only by the one blocking lockf; reserve full case bound, then run once."""
from pathlib import Path
import hashlib
import json
import os
import subprocess
import time

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-runtime-v2")
controller = root / "controller"
ready = json.loads((controller / "ready-v2.json").read_text())
budget = ready["budget"]
now = time.time()
assert now + 45 < budget["deadline_epoch"]
usage = os.statvfs(root)
free = usage.f_bavail * usage.f_frsize
assert free >= 34 * 1024**3
assert hashlib.sha256(Path(ready["executable"]).read_bytes()).hexdigest() == ready["sha256"] == "ce4c34d7f480c8b02073d15e0f4ba5b2a11ebfe5b2ceb8bfe2fdc996cc2a8871"
assert hashlib.sha256((root / "frozen-inputs/source-package.tar.gz").read_bytes()).hexdigest() == ready["source_package_sha256"]
assert not (root / "native-records-v2").exists()
(root / "native-tmp-v2").mkdir()
receipt = {"status": "LockAcquired_AllIdentitiesRechecked_BeforeSoleActor", "at_epoch": time.time(),
           "free_bytes": free, "ready": ready, "lock": "/tmp/g121-mac.lock", "full_parent_seconds": 45,
           "work_seconds": 30, "cleanup_seconds": 15, "runtime_attempts_so_far": 0,
           "lock_command": json.loads((controller / "lock.command.json").read_text())}
with (controller / "preactor-after-lock.json").open("x") as file:
    json.dump(receipt, file, indent=2)
assert time.time() + 45 < budget["deadline_epoch"]
with (controller / "actor-start.json").open("x") as file:
    json.dump({"epoch": time.time(), "runtime_attempt": 1, "argv": ready["argv"],
               "deadline_epoch": budget["deadline_epoch"]}, file, indent=2)
env = os.environ.copy()
for key in list(env):
    if key.startswith("DYLD_"):
        del env[key]
env.update(ready["case_environment"])
start = time.time()
with (controller / "case.stdout").open("xb") as out, (controller / "case.stderr").open("xb") as err:
    try:
        result = subprocess.run(ready["argv"], cwd=root, env=env, stdout=out, stderr=err, timeout=45)
        code, expired = result.returncode, False
    except subprocess.TimeoutExpired:
        code, expired = None, True
outcome = {"exit_code": code, "timeout": expired, "elapsed_s": time.time() - start,
           "runtime_attempts": 1, "runtime_retries": 0, "original_deadline_epoch": budget["deadline_epoch"],
           "cleanup": "Unknown_if_external_timeout_otherwise_actual_leaf_records_authoritative"}
with (controller / "case.result.json").open("x") as file:
    json.dump(outcome, file, indent=2)
print(json.dumps(outcome), flush=True)
raise SystemExit(0 if code == 0 and not expired else 1)
