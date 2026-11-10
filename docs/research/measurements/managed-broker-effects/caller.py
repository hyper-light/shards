#!/usr/bin/env python3
"""One fresh native case; process timeout is a bound, never a passing receipt."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

TEST = "broker_effects::native_program_results_have_private_capture_and_owned_worker_cleanup"


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for key in ("source", "revision", "binary", "records", "guests", "leaf_sha"):
        p.add_argument("--" + key.replace("_", "-"), required=True)
    for key in ("case_ms", "call_ms", "cleanup_ms", "timeout_ms", "cancel_work_ms",
                "capture_bytes", "outer_ms", "reap_ms"):
        p.add_argument("--" + key.replace("_", "-"), type=int, required=True)
    args = p.parse_args()
    assert all(getattr(args, k) > 0 for k in ("case_ms", "call_ms", "cleanup_ms", "timeout_ms",
                                             "cancel_work_ms", "capture_bytes", "outer_ms", "reap_ms"))
    assert args.cleanup_ms < args.case_ms < args.outer_ms
    source, binary, records, guests = map(lambda x: Path(x).resolve(),
                                          (args.source, args.binary, args.records, args.guests))
    records.mkdir(exist_ok=False)
    leaf = source / "crates/shards/tests/managed_output/broker_effects.rs"
    revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    assert revision == args.revision
    assert sha(leaf) == args.leaf_sha
    env = dict(os.environ, SHARDS_REQUIRE_VMS="1", SHARDS_TEST_GUESTS=str(guests),
               XDG_CACHE_HOME=str(records / "runtime-cache"),
               SHARDS_CACHE=str(records / "runtime-cache/shards"))
    for key in ("case_ms", "call_ms", "cleanup_ms", "timeout_ms", "cancel_work_ms", "capture_bytes"):
        env["SHARDS_BROKER_EFFECTS_" + key.upper()] = str(getattr(args, key))
    env["SHARDS_BROKER_EFFECTS_RECORDS"] = str(records / "native")
    command = [str(binary), TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
    (records / "association.json").write_text(json.dumps({
        "base": revision, "leaf_sha256": sha(leaf), "binary_sha256": sha(binary),
        "binary_bytes": binary.stat().st_size, "parent_sha256": sha(source / "crates/shards/tests/managed_output.rs"),
        "caller_sha256": sha(Path(__file__)), "command": command, "inputs": vars(args),
        "limit": "source hashes associate recorded inputs; build logs must associate actual binary",
    }, indent=2))
    started = time.monotonic()
    child = subprocess.Popen(command, cwd=source, env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, start_new_session=True)
    timed_out, signals = False, []
    try:
        output, _ = child.communicate(timeout=args.outer_ms / 1000)
    except subprocess.TimeoutExpired as error:
        timed_out = True
        output = error.output or b""
        try:
            os.killpg(child.pid, signal.SIGKILL)
            signals.append(int(signal.SIGKILL))
        except ProcessLookupError:
            pass
        try:
            output, _ = child.communicate(timeout=args.reap_ms / 1000)
        except subprocess.TimeoutExpired as error:
            output = (error.output or output) + b"\nouter reap deadline: process/pipe cleanup unknown\n"
    (records / "native.log").write_bytes(output)
    passed = (not timed_out and child.returncode == 0
              and any(line.startswith(b"test result: ok. 1 passed; 0 failed; 0 ignored;")
                      for line in output.splitlines()))
    result = {"exit_code": child.returncode, "outer_timeout": timed_out, "signals": signals,
              "elapsed_seconds": time.monotonic() - started, "passed": passed,
              "cleanup_limit": "process-group bound only; private daemon/VM return requires native cleanup receipt; timeout leaves unknowns"}
    (records / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
