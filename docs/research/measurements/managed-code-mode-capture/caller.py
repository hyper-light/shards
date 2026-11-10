#!/usr/bin/env python3
"""Run the one ordinary legacy test with checked compilation inputs and finite bounds."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

TEST = "code_mode_runs_a_program_with_the_callers_grants"
PATHS = {"crates/shards/tests/common/managed_capture.rs",
         "crates/shards/tests/managed_output.rs", "crates/shards/tests/build.rs"}


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("source", "binary", "records", "guests", "kernel", "inputs"):
        parser.add_argument("--" + key, required=True)
    parser.add_argument("--outer-ms", required=True, type=int)
    parser.add_argument("--reap-ms", required=True, type=int)
    args = parser.parse_args()
    assert 0 < args.outer_ms <= 240000 and 0 < args.reap_ms <= 15000
    source, binary, records, guests, kernel, inputs = [Path(getattr(args, key)).resolve()
        for key in ("source", "binary", "records", "guests", "kernel", "inputs")]
    expected = json.loads(inputs.read_text())
    assert set(expected["source_hashes"]) == PATHS
    revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    assert revision == expected["revision"]
    changed = subprocess.check_output(["git", "-C", str(source), "diff", "--name-only"], text=True).splitlines()
    assert set(changed) == PATHS - {"crates/shards/tests/common/managed_capture.rs"}
    actual = {path: sha(source / path) for path in PATHS}
    assert actual == expected["source_hashes"]
    assert sha(binary) == expected["binary_sha256"]
    assert sha(guests / "guest/shards-testguest") == expected["guest_sha256"]
    assert sha(kernel) == expected["kernel_sha256"]
    records.mkdir(exist_ok=False)
    env = dict(os.environ, SHARDS_REQUIRE_VMS="1", SHARDS_TEST_GUESTS=str(guests),
               SHARDS_TEST_KERNEL=str(kernel), SHARDS_CODE_MODE_CAPTURE_RECORDS=str(records / "native"),
               XDG_CACHE_HOME=str(records / "runtime-cache"), SHARDS_CACHE=str(records / "runtime-cache/shards"))
    env.pop("SHARDS_VM_BINARY", None)
    command = [str(binary), TEST, "--exact", "--nocapture", "--test-threads=1"]
    (records / "association.json").write_text(json.dumps({
        "revision": revision, "source_hashes": actual, "expected_inputs": expected,
        "inputs_sha256": sha(inputs), "binary_sha256": sha(binary), "binary_bytes": binary.stat().st_size,
        "guest_sha256": sha(guests / "guest/shards-testguest"), "kernel_sha256": sha(kernel),
        "caller_sha256": sha(Path(__file__)), "command": command, "arguments": vars(args),
        "test_bounds": "one ordinary test,120s original work deadline+at most15s cleanup",
    }, indent=2))
    started = time.monotonic()
    child = subprocess.Popen(command, cwd=source, env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, start_new_session=True)
    timed_out, signals = False, []
    try:
        output, _ = child.communicate(timeout=args.outer_ms / 1000)
    except subprocess.TimeoutExpired as error:
        timed_out, output = True, error.output or b""
        try:
            os.killpg(child.pid, signal.SIGKILL)
            signals.append(int(signal.SIGKILL))
        except ProcessLookupError:
            pass
        try:
            output, _ = child.communicate(timeout=args.reap_ms / 1000)
        except subprocess.TimeoutExpired as error:
            output = (error.output or output) + b"\nouter reap deadline: cleanup unknown\n"
    (records / "native.log").write_bytes(output)
    cleanup_path = records / "native/cleanup.json"
    cleanup = json.loads(cleanup_path.read_text()) if cleanup_path.exists() else None
    passed = (not timed_out and child.returncode == 0
              and any(line.startswith(b"test result: ok. 1 passed; 0 failed; 0 ignored;")
                      for line in output.splitlines())
              and cleanup is not None and cleanup["body_passed"]
              and cleanup["owner_stop_observer_join_errors"] == [])
    result = {"exit_code": child.returncode, "outer_timeout": timed_out, "signals": signals,
              "passed": passed, "elapsed_seconds": time.monotonic() - started,
              "cleanup_limit": "process-group reap bound only; full kernel return not inferred"}
    (records / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
