#!/usr/bin/env python3
"""Run only the scoped native units/lints under the caller's Kiln envelope."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--harness", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("--boundary", action="store_true")
    args = parser.parse_args()
    if args.seconds <= 0:
        parser.error("a positive caller deadline is required")
    root = Path(__file__).resolve().parents[4]
    args.output.mkdir(parents=True, exist_ok=False)
    environment = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
    commands = [
        ("remote-units", ["cargo", "test", "--locked", "-p", "shards-remote", "--lib", "managed::", "-j2", "--", "--test-threads=1"]),
        ("daemon-core-units", ["cargo", "test", "--locked", "--manifest-path", str(args.harness / "Cargo.toml"), "--lib", "daemon::managed::terminal_tests::", "-j2", "--", "--test-threads=1"]),
        ("remote-clippy", ["cargo", "clippy", "--locked", "-p", "shards-remote", "--lib", "-j2", "--", "-D", "warnings"]),
        ("daemon-core-clippy", ["cargo", "clippy", "--locked", "--manifest-path", str(args.harness / "Cargo.toml"), "--lib", "-j2", "--", "-D", "warnings"]),
    ]
    if args.boundary:
        commands = [
            ("producer-boundary", ["cargo", "test", "--manifest-path", str(args.harness / "Cargo.toml"), "--test", "producer_boundary", "-j2", "--", "--exact", "a_small_owned_submission_cannot_lend_less_storage_than_its_next_credit", "--nocapture", "--test-threads=1"]),
        ]
    sources = ["crates/remote/src/managed.rs", "crates/shards/src/daemon/managed.rs", "Cargo.lock"]
    hashes = lambda: {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources}
    before = hashes()
    deadline = time.monotonic() + args.seconds
    results = []
    for name, command in commands:
        began = time.monotonic()
        with (args.output / (name + ".stdout")).open("wb") as out, (args.output / (name + ".stderr")).open("wb") as err:
            try:
                result = subprocess.run(command, cwd=root, env=environment, stdout=out, stderr=err, timeout=max(0, deadline - began))
                status = result.returncode
            except subprocess.TimeoutExpired:
                status = "caller-deadline"
        results.append({"name": name, "command": command, "status": status, "elapsed_seconds": time.monotonic() - began})
        if status != 0:
            break
    record = {"source_before": before, "source_after": hashes(), "checks": results}
    (args.output / "receipt.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2))
    raise SystemExit(0 if len(results) == len(commands) and all(r["status"] == 0 for r in results) and record["source_after"] == before else 1)


if __name__ == "__main__":
    main()
