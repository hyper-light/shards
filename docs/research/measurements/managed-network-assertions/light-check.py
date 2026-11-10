#!/usr/bin/env python3
"""Compile the complete host fixture with inert VM/common stubs; run pure tests only."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


STUBS = r"""mod common {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub struct Run {
        pub status: Option<i32>,
        pub stdout: String,
        pub stderr: String,
    }
    impl std::fmt::Display for Run {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "inert common stub: {:?}: {} / {}", self.status, self.stdout, self.stderr)
        }
    }
    pub struct TempDir(PathBuf);
    impl TempDir {
        pub fn new(_: &str) -> Self { panic!("VM stub must never execute") }
    }
    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path { &self.0 }
    }
    pub fn sha256_digest(_: &[u8]) -> String { panic!("VM stub must never execute") }
    pub fn test_guest() -> &'static Path { panic!("VM stub must never execute") }
    pub fn shards() -> &'static Path { panic!("VM stub must never execute") }
    pub fn cannot_run_vms() -> bool { panic!("VM stub must never execute") }
    pub fn run_shards_env<S: AsRef<std::ffi::OsStr>>(
        _: &[&str], _: &[&str], _: &[(&str, S)], _: Duration,
    ) -> Run { panic!("VM stub must never execute") }
}"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("work", type=Path)
    args = parser.parse_args()
    source = args.source.read_bytes()
    text = source.decode()
    if text.count("mod common;") != 1:
        raise SystemExit("expected exactly one common module declaration")
    args.work.mkdir(parents=True, exist_ok=False)
    (args.work / "src").mkdir()
    (args.work / "src/lib.rs").write_text(text.replace("mod common;", STUBS))
    (args.work / "Cargo.toml").write_text(
        '[package]\nname = "managed-network-assertions-light"\n'
        'version = "0.0.0"\nedition = "2024"\n'
        '[dependencies]\nserde_json = "=1.0.151"\n'
    )
    command = [
        "cargo", "test", "--offline", "--manifest-path", str(args.work / "Cargo.toml"),
        "--target-dir", str(args.work / "target"), "--target", "x86_64-unknown-linux-gnu",
        "--", "--test-threads=1",
    ]
    environment = dict(os.environ, CARGO_BUILD_JOBS="1")
    print(json.dumps({"source_sha256": hashlib.sha256(source).hexdigest(),
                      "scope": "complete extracted host fixture; inert common stubs; pure tests only",
                      "command": command, "cargo_build_jobs": 1}), flush=True)
    result = subprocess.run(command, env=environment, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, check=False)
    (args.work / "cargo-test.log").write_text(result.stdout)
    print(result.stdout, end="", flush=True)
    print(json.dumps({"exit_code": result.returncode}), flush=True)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
