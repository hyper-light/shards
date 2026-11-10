#!/usr/bin/env python3
"""Compile exact pure parser slices plus actual ABI; state-cases.rs stays unrun."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def between(source, start, end):
    if source.count(start) != 1 or source.count(end) != 1:
        raise ValueError(f"source extraction marker is not unique: {start!r}, {end!r}")
    first = source.index(start)
    last = source.index(end, first)
    return source[first:last]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("work", type=Path)
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    repo, work = args.repo.resolve(), args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    shutil.copytree(repo / "crates/abi", work / "crates/abi")
    source = Path(__file__).resolve().parent
    remote = (repo / "crates/remote/src/managed.rs").read_text()
    frame = (repo / "crates/remote/src/frame.rs").read_text()
    library = (repo / "crates/remote/src/lib.rs").read_text()
    root = (repo / "Cargo.toml").read_text()
    package = between(root, "[workspace.package]", "[workspace.dependencies]")
    lints = between(root, "[workspace.lints.rust]", "[patch.crates-io]")
    (work / "Cargo.toml").write_text(
        '[workspace]\nresolver = "3"\nmembers = ["crates/abi", "parser"]\n' + package + lints)
    (work / "parser/src").mkdir(parents=True)
    (work / "parser/Cargo.toml").write_text(
        '[package]\nname = "managed-operator-boundaries"\nversion = "0.0.0"\n'
        'edition = "2024"\n[dependencies]\nshards-abi = { path = "../crates/abi" }\n')
    # Exact source slices; no parser, enum or state logic is reimplemented.
    error = between(library, "/// What went wrong, said.", "#[cfg(test)]")
    tags = between(frame, "/// The most a frame carries:", "/// `payload` framed")
    peek = between(frame, "/// What the frame at the front", "/// The first whole frame")
    types = between(remote, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum ProtocolError", "pub const ALPN:")
    constants = between(remote, "const FRAME_HEAD:", "/// A run and its current management")
    protocol = between(remote, "/// A run and its current management", "/// The caller retains a host resource")
    cases = (source / "parser-cases.rs").read_text()
    generated = ('#![allow(dead_code)]\n' + error + '\nmod frame {\nuse crate::Error;\n'
                 + tags + peek + '}\nmod managed {\nuse std::mem::size_of;\n'
                 'use shards_abi::run::managed::Channel;\nuse crate::frame::{self, Tag};\n'
                 + types + constants + protocol + '\n#[cfg(test)]\nmod tests {\nuse super::*;\n'
                 + cases + '\n}\n}\n')
    (work / "parser/src/lib.rs").write_text(generated)
    environment = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    environment["CARGO_BUILD_JOBS"] = "1"
    results = {"scope": "actual extracted parser and ABI only; state-cases, Service, controls dispatch, I/O and transport are NOT compiled/executed",
               "source_hashes": {name: hashlib.sha256((repo / name).read_bytes()).hexdigest()
                                 for name in ["crates/remote/src/lib.rs", "crates/remote/src/frame.rs", "crates/remote/src/managed.rs"]},
               "generated_sha256": hashlib.sha256(generated.encode()).hexdigest(), "commands": []}
    commands = [["rustc", "-vV"], ["cargo", "test", "--offline", "--manifest-path", str(work / "Cargo.toml"),
                 "--target-dir", str(work / "target"), "--target", args.target,
                 "-p", "managed-operator-boundaries", "--lib", "--", "--test-threads=1"]]
    for index, command in enumerate(commands):
        result = subprocess.run(command, cwd=work, env=environment, capture_output=True, check=False)
        log = result.stdout + result.stderr
        (work / f"command-{index}.log").write_bytes(log)
        results["commands"].append({"argv": command, "exit_code": result.returncode,
                                    "log_sha256": hashlib.sha256(log).hexdigest()})
        (work / "result.json").write_text(json.dumps(results, indent=2) + "\n")
        print(log.decode(errors="replace"), end="", flush=True)
        if result.returncode:
            return result.returncode
    print(json.dumps(results), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
