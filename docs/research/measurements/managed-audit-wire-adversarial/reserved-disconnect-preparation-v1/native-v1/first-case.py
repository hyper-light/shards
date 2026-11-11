#!/usr/bin/env python3
"""Run the sole previously recorded native dispatch once under its original bound."""
from pathlib import Path
import json
import os
import subprocess
import time

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-v1")
records = root / "case-controller-v1"
dispatch = json.loads((records / "dispatch-before-case.json").read_text())
budget, ready = dispatch["budget"], dispatch["ready"]
assert time.time() + 45 < budget["deadline_epoch"]
usage = os.statvfs(root)
assert usage.f_bavail * usage.f_frsize >= 30 * 1024**3
with (records / "actor-start.json").open("x") as file:
    json.dump({"epoch": time.time(), "native_case_attempt": 1, "argv": ready["argv"],
               "deadline_epoch": budget["deadline_epoch"]}, file, indent=2)
env = os.environ.copy()
for key in list(env):
    if key.startswith("DYLD_"):
        del env[key]
env.update(ready["case_environment"])
start = time.time()
with (records / "case.stdout").open("xb") as out, (records / "case.stderr").open("xb") as err:
    try:
        child = subprocess.run(ready["argv"], cwd=root / "source", env=env, stdout=out, stderr=err, timeout=45)
        code, expired = child.returncode, False
    except subprocess.TimeoutExpired:
        code, expired = None, True
result = {"exit_code": code, "timeout": expired, "elapsed_s": time.time() - start,
          "native_case_attempts": 1, "native_retries": 0, "original_deadline_epoch": budget["deadline_epoch"],
          "cleanup": "Unknown_if_parent_timeout_otherwise_leaf_receipts_authoritative"}
with (records / "case.result.json").open("x") as file:
    json.dump(result, file, indent=2)
print(json.dumps(result), flush=True)
print("STDOUT\n" + (records / "case.stdout").read_text(), flush=True)
print("STDERR\n" + (records / "case.stderr").read_text(), flush=True)
events = root / "native-records-v1/events.log"
if events.exists():
    print("EVENTS\n" + events.read_text(), flush=True)
raise SystemExit(0 if code == 0 and not expired else 1)
