#!/usr/bin/env python3
"""One event-driven lock acquisition; wait bound derives from the absolute budget."""
from pathlib import Path
import json
import math
import subprocess
import time

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-runtime-v2")
controller = root / "controller"
budget = json.loads((root / "budget.json").read_text())
start = time.time()
wait_seconds = math.floor(budget["deadline_epoch"] - start - 45)
assert wait_seconds >= 0
argv = ["lockf", "-t", str(wait_seconds), "/tmp/g121-mac.lock", "python3", str(Path(__file__).parent / "after-lock.py")]
with (controller / "lock.command.json").open("x") as file:
    json.dump({"argv": argv, "at_epoch": start, "wait_allowance_seconds": wait_seconds,
               "derived_from": "floor(absolute_deadline - now - unchanged45s)",
               "deadline_epoch": budget["deadline_epoch"], "runtime_retries": 0}, file, indent=2)
with (controller / "lock.stdout").open("xb") as out, (controller / "lock.stderr").open("xb") as err:
    try:
        result = subprocess.run(argv, cwd=root, stdout=out, stderr=err, timeout=budget["deadline_epoch"] - time.time())
        code, expired = result.returncode, False
    except subprocess.TimeoutExpired:
        code, expired = None, True
outcome = {"exit_code": code, "timeout": expired, "elapsed_s": time.time() - start,
           "actor_start_present": (controller / "actor-start.json").exists(),
           "original_deadline_epoch": budget["deadline_epoch"]}
with (controller / "lock.result.json").open("x") as file:
    json.dump(outcome, file, indent=2)
print(json.dumps(outcome), flush=True)
print((controller / "lock.stdout").read_text(), flush=True)
print((controller / "lock.stderr").read_text(), flush=True)
raise SystemExit(0 if code == 0 and not expired else 1)
