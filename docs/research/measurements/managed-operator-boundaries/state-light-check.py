#!/usr/bin/env python3
"""Reproduce the five prepared pure cases in the actual remote test module."""

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
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    repo, work = args.repo.resolve(), args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    source = work / "source"
    source.mkdir()
    for name in ["abi", "ipc", "remote", "testdir"]:
        shutil.copytree(repo / "crates" / name, source / "crates" / name)
    for name in ["aws-lc-rs", "aws-lc-sys", "rquickjs-sys"]:
        shutil.copytree(repo / "vendor" / name, source / "vendor" / name)
    shutil.copytree(repo / ".cargo", source / ".cargo")
    shutil.copyfile(repo / "clippy.toml", source / "clippy.toml")
    shutil.copyfile(repo / "rustfmt.toml", source / "rustfmt.toml")
    shutil.copyfile(repo / "Cargo.lock", source / "Cargo.lock")
    root = (repo / "Cargo.toml").read_text()
    (source / "Cargo.toml").write_text(
        '[workspace]\nresolver = "3"\nmembers = ["crates/abi", "crates/ipc", '
        '"crates/remote", "crates/testdir"]\n' + root[root.index("[workspace.package]"):])
    owned = Path(__file__).resolve().parent
    path = source / "crates/remote/src/managed.rs"
    original = path.read_text()
    marker = "mod tests {\n    use super::*;"
    if original.count(marker) != 1:
        raise ValueError("existing test module marker must be unique")
    cases = {name: (owned / name).read_text() for name in ["parser-cases.rs", "state-cases.rs"]}
    inserted = ('\n    #[allow(clippy::indexing_slicing)]\n    mod operator_boundaries {\n'
                '        use super::*;\n' + cases["parser-cases.rs"] + '\n'
                + cases["state-cases.rs"] + '\n    }\n')
    path.write_text(original.replace(marker, marker + inserted))
    environment = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    environment.update(CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0",
                       CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0")
    common = ["--offline", "-p", "shards-remote", "--target", args.target,
              "--target-dir", str(work / "target"), "--jobs", "1"]
    commands = [
        ["cargo", "test", *common, "--lib", "managed::tests::operator_boundaries", "--", "--test-threads=1"],
        ["cargo", "clippy", *common, "--lib", "--tests", "--", "-D", "warnings"],
    ]
    results = {"original_source_sha256": hashlib.sha256(original.encode()).hexdigest(),
               "private_test_source_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
               "case_hashes": {n: hashlib.sha256(s.encode()).hexdigest() for n, s in cases.items()},
               "scope": "actual full remote module, only five prepared pure cases execute; no transport/VM",
               "commands": []}
    for index, command in enumerate(commands):
        result = subprocess.run(command, cwd=source, env=environment, capture_output=True, check=False)
        log = result.stdout + result.stderr
        (work / f"command-{index}.log").write_bytes(log)
        results["commands"].append({"argv": command, "exit_code": result.returncode,
                                    "log_sha256": hashlib.sha256(log).hexdigest()})
        (work / "result.json").write_text(json.dumps(results, indent=2) + "\n")
        print(log.decode(errors="replace"), end="", flush=True)
        if result.returncode:
            return result.returncode
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
