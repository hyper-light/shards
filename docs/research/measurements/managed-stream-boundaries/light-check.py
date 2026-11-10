#!/usr/bin/env python3
"""Run actual ABI library tests and scoped lints in a fresh private workspace."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("work", type=Path)
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=False)
    shutil.copytree(args.repo / "crates/abi", args.work / "crates/abi")
    shutil.copytree(args.repo / ".cargo", args.work / ".cargo")
    shutil.copyfile(args.repo / "clippy.toml", args.work / "clippy.toml")
    root = (args.repo / "Cargo.toml").read_text()
    package = root[root.index("[workspace.package]"):root.index("[workspace.dependencies]")]
    lints = root[root.index("[workspace.lints.rust]"):root.index("[patch.crates-io]")]
    (args.work / "Cargo.toml").write_text(
        '[workspace]\nresolver = "3"\nmembers = ["crates/abi"]\n' + package + lints
    )
    environment = dict(os.environ, CARGO_BUILD_JOBS="1")
    results = []
    for target in ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"]:
        for action in ["test", "clippy"]:
            command = ["cargo", action, "--offline", "-p", "shards-abi", "--lib",
                       "--manifest-path", str(args.work / "Cargo.toml"),
                       "--target-dir", str(args.work / "target"), "--target", target]
            command += ["--", "--test-threads=1"] if action == "test" else ["--tests", "--", "-D", "warnings"]
            result = subprocess.run(command, env=environment, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, text=True, check=False)
            log = f"{target}-{action}.log"
            (args.work / log).write_text(result.stdout)
            results.append({"command": command, "exit_code": result.returncode, "log": log,
                            "log_sha256": hashlib.sha256(result.stdout.encode()).hexdigest()})
            print(json.dumps(results[-1]), flush=True)
            print(result.stdout, end="", flush=True)
            if result.returncode:
                break
        if result.returncode:
            break
    (args.work / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
