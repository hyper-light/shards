#!/usr/bin/env python3
"""Pure-only check in a NEW private directory. Never builds/runs fixture.rs."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("work", type=Path)
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=False)
    target = args.work / "target"
    target.mkdir()
    shutil.copyfile(args.source / "accounting.rs", args.work / "accounting.rs")
    results = {"system": platform.platform(), "machine": platform.machine(),
               "source_sha256": hashlib.sha256((args.work / "accounting.rs").read_bytes()).hexdigest(),
               "target": args.target, "commands": []}
    commands = [
        ["rustc", "-vV"],
        ["rustc", "--test", "--edition", "2024", "--target", args.target,
         str(args.work / "accounting.rs"), "-o", str(target / "accounting-tests")],
        [str(target / "accounting-tests"), "--test-threads=1"],
    ]
    environment = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    for index, command in enumerate(commands):
        result = subprocess.run(command, capture_output=True, env=environment, check=False)
        log = result.stdout + result.stderr
        path = args.work / f"command-{index}.log"
        path.write_bytes(log)
        results["commands"].append({"argv": command, "exit_code": result.returncode,
                                    "log": str(path), "log_sha256": hashlib.sha256(log).hexdigest()})
        (args.work / "result.json").write_text(json.dumps(results, indent=2) + "\n")
        print(log.decode(errors="replace"), end="", flush=True)
        if result.returncode:
            return result.returncode
    print(json.dumps(results), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
