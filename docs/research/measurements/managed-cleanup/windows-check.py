"""Type-check the actual Windows client/adapter/native tests without C/SDK builds.

This is a narrow compile diagnostic, not the full shards Windows gate or native
execution. It extracts actual source and pins the existing windows-sys lock entry.
"""
import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import tomllib

parser = argparse.ArgumentParser()
parser.add_argument("--evidence", type=pathlib.Path, required=True)
parser.add_argument("--target", required=True)
args = parser.parse_args()
if args.target not in {"x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"}:
    raise SystemExit("expected a supported Windows target")
args.evidence.mkdir(parents=True, exist_ok=False)
root = pathlib.Path(__file__).resolve().parents[4]
host = root / "crates/shards/tests/managed_cleanup.rs"
source = host.read_text()
core = source[source.index("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\nenum Phase"):source.index("fn ready(home:")]
native = source[source.index("mod native {"):]
prefix = """#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
"""
project = args.evidence / "project"
(project / "src").mkdir(parents=True)
actual = prefix + core + native
(project / "src/lib.rs").write_text(actual)
workspace = tomllib.loads((root / "Cargo.toml").read_text())
shards = tomllib.loads((root / "crates/shards/Cargo.toml").read_text())
lock = tomllib.loads((root / "Cargo.lock").read_text())
version = next(p["version"] for p in lock["package"] if p["name"] == "windows-sys" and p["version"].startswith("0.61."))
features = set(workspace["workspace"]["dependencies"]["windows-sys"]["features"])
for target in shards["target"].values():
    for section in ["dependencies", "dev-dependencies"]:
        features.update(target.get(section, {}).get("windows-sys", {}).get("features", []))
manifest = f'''[package]
name = "cleanup-windows-typecheck"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
shards-ipc = {{ path = {json.dumps(str(root / "crates/ipc"))} }}
windows-sys = {{ version = "={version}", features = {json.dumps(sorted(features))} }}
'''
(project / "Cargo.toml").write_text(manifest)
record = {"host_source_sha256": hashlib.sha256(source.encode()).hexdigest(),
          "extracted_source_sha256": hashlib.sha256(actual.encode()).hexdigest(),
          "rustc": subprocess.check_output(["rustc", "-Vv"], cwd=root, text=True),
          "cwd": str(root), "target": args.target, "windows_sys": version,
          "target_dir": os.environ.get("CARGO_TARGET_DIR"), "scope": "actual Client/adapter/native tests only"}
(args.evidence / "identity.json").write_text(json.dumps(record, indent=2) + "\n")
generate = ["cargo", "generate-lockfile", "--offline", "--manifest-path", str(project / "Cargo.toml")]
with (args.evidence / "lock.log").open("w") as output:
    code = subprocess.call(generate, cwd=root, stdout=output, stderr=subprocess.STDOUT)
(args.evidence / "lock.exit").write_text(str(code) + "\n")
if code:
    raise SystemExit(code)
command = ["cargo", "check", "--offline", "--locked", "-j4", "--target", args.target,
           "--tests", "--manifest-path", str(project / "Cargo.toml")]
(args.evidence / "command.json").write_text(json.dumps(command, indent=2) + "\n")
with (args.evidence / "check.log").open("w") as output:
    code = subprocess.call(command, cwd=root, stdout=output, stderr=subprocess.STDOUT)
(args.evidence / "check.exit").write_text(str(code) + "\n")
print((args.evidence / "check.log").read_text())
raise SystemExit(code)
