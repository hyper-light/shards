#!/usr/bin/env python3
"""One fresh admitted light batch. Preserve all inputs, logs and frozen binaries."""
from pathlib import Path
import hashlib
import json
import os
import signal
import subprocess
import sys
import time

root = Path(__file__).resolve().parent
output = root / "results"
output.mkdir(exist_ok=False)
started = time.monotonic()
# The coordinator-assigned batch is absolute, including compiler, cases and cleanup.
deadline = started + 600
env = os.environ.copy()
env["CARGO_BUILD_JOBS"] = "2"
env["CARGO_TARGET_DIR"] = str(root / "target")
env["TMPDIR"] = str(root / "private-temp")
Path(env["TMPDIR"]).mkdir(mode=0o700)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


manifest = json.loads((root / "SOURCE-MANIFEST.json").read_text())
for entry in manifest:
    path = root / entry["path"]
    if path.stat().st_size != entry["bytes"] or digest(path) != entry["sha256"]:
        raise RuntimeError("frozen source mismatch: " + entry["path"])

environment = {
    "uname": list(os.uname()),
    "affinity_before": sorted(os.sched_getaffinity(0)),
    "batch_seconds": 600,
    "cargo_jobs": 2,
    "source_manifest_sha256": digest(root / "SOURCE-MANIFEST.json"),
    "scope": "two ordinary private child status reports; no owner/cgroup/VM authority",
}
affinity = environment["affinity_before"]
if len(affinity) < 2:
    raise RuntimeError("assigned two-CPU batch lacks two available CPUs")
os.sched_setaffinity(0, set(affinity[:2]))
environment["affinity_effective"] = sorted(os.sched_getaffinity(0))
(output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")

stages = []


def run(name, argv, cwd=None):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("absolute batch expired before " + name)
    record = {"name": name, "argv": argv, "remaining_seconds_before": remaining}
    start = time.monotonic()
    with (output / (name + ".stdout")).open("wb") as out, (output / (name + ".stderr")).open("wb") as err:
        process = subprocess.Popen(argv, cwd=cwd or root / "harness", env=env,
                                   stdout=out, stderr=err, start_new_session=True)
        try:
            code = process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            record["timed_out"] = True
            record["cleanup"] = "driver direct child reaped; fixture population Unknown"
            code = process.returncode
    record.update(exit=code, seconds=time.monotonic() - start)
    stages.append(record)
    (output / "stages.json").write_text(json.dumps(stages, indent=2) + "\n")
    if code != 0:
        raise RuntimeError("stage failed: " + name)
    return (output / (name + ".stdout")).read_text()


outcome = "failed"
try:
    run("rustc", ["rustc", "--version", "--verbose"])
    run("cargo", ["cargo", "--version", "--verbose"])
    text = run("build", ["cargo", "test", "--locked", "--offline", "--no-run", "--jobs", "2", "--message-format=json"])
    binaries = []
    for line in text.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("profile", {}).get("test") and message.get("executable"):
            binaries.append(message["executable"])
    if len(binaries) != 1:
        raise RuntimeError("expected one exact test binary")
    original = Path(binaries[0])
    frozen = output / "frozen-tests"
    frozen.write_bytes(original.read_bytes())
    frozen.chmod(0o500)
    (output / "pre-actor.json").write_text(json.dumps({
        "path": str(frozen), "bytes": frozen.stat().st_size,
        "sha256": digest(frozen), "source_manifest_sha256": digest(root / "SOURCE-MANIFEST.json"),
        "expected_native_cases": 2, "prior_cases": 0,
    }, indent=2) + "\n")
    listing = run("list", [str(frozen), "status_placement::", "--list"])
    if "2 tests, 0 benchmarks" not in listing or listing.count(": test") != 2:
        raise RuntimeError("the exact two-case fixture list differs")
    run("native", [str(frozen), "status_placement::", "--test-threads=1", "--nocapture"])
    run("clippy", ["cargo", "clippy", "--locked", "--offline", "--all-targets", "--jobs", "2", "--", "-D", "warnings"])
    for entry in manifest:
        path = root / entry["path"]
        if path.stat().st_size != entry["bytes"] or digest(path) != entry["sha256"]:
            raise RuntimeError("source changed during batch: " + entry["path"])
    outcome = "passed"
except Exception as error:
    (output / "failure.txt").write_text(type(error).__name__ + ": " + str(error) + "\n")
finally:
    summary = {"outcome": outcome, "seconds": time.monotonic() - started,
               "stages": stages, "production_integration": "unapplied caller hook; native lead owns it",
               "kernel_population": "Unknown", "authority": "MechanicsOnly"}
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary))
sys.exit(0 if outcome == "passed" else 1)
