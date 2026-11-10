#!/usr/bin/env python3
"""One native Windows light pass in the assigned private clone and target."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

PIN = "122332036eb689c33fd52b4a065c53c7dc5e4530"
source = Path.cwd()
private = Path.home() / "managed-borrowed-windows-check"
target = Path.home() / "managed-borrowed-windows-target"
evidence = Path.home() / "managed-borrowed-windows-evidence" / PIN[:8]
evidence.mkdir(parents=True, exist_ok=True)
shutil.copyfile(__file__, evidence / "driver.py")


def command(name, args, cwd=private):
    with (evidence / f"{name}.log").open("w", encoding="utf-8") as log:
        result = subprocess.run(args, cwd=cwd, stdout=log, stderr=subprocess.STDOUT)
    (evidence / f"{name}.json").write_text(json.dumps({"args": args, "cwd": str(cwd), "exit": result.returncode}, indent=2))
    print(f"{name}: exit={result.returncode}", flush=True)
    return result.returncode


if platform.system() != "Windows":
    raise RuntimeError("This is the assigned native Windows check, not a cross build")
if not private.exists():
    if command("clone", ["git", "clone", "--no-hardlinks", "--no-checkout", str(source), str(private)], source):
        sys.exit(1)
    if command("checkout", ["git", "checkout", "--detach", PIN]):
        sys.exit(1)
head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=private, text=True).strip()
dirty = subprocess.check_output(["git", "status", "--porcelain"], cwd=private, text=True)
if head != PIN or dirty:
    raise RuntimeError("Private clone is not the assigned clean source; refusing mutation")
os.environ["CARGO_TARGET_DIR"] = str(target)
identity = {"head": head, "source": str(private), "target": str(target), "evidence": str(evidence), "platform": platform.platform(), "python": sys.version, "machine": platform.machine()}
(evidence / "identity.json").write_text(json.dumps(identity, indent=2))
print(json.dumps(identity), flush=True)
command("rustc", ["rustc", "-Vv"])
command("cargo", ["cargo", "-Vv"])
paths = ["Cargo.lock", "crates/api/src/borrowed.rs", "crates/api/src/channel.rs", "crates/api/src/stream.rs", "crates/afunix/src/lib.rs"]
(evidence / "source.sha256").write_text("\n".join(f"{hashlib.sha256((private / p).read_bytes()).hexdigest()}  {p}" for p in paths) + "\n")

build = command("build", ["cargo", "test", "--locked", "-j4", "--profile", "test-release", "-p", "shards-api", "--lib", "--no-run", "--message-format=json-render-diagnostics"])
if build:
    print(f"COMPILE_RED: {evidence / 'build.log'}", flush=True)
    sys.exit(build)
executables = []
for line in (evidence / "build.log").read_text(encoding="utf-8").splitlines():
    if not line.startswith("{"):
        continue
    row = json.loads(line)
    if row.get("reason") == "compiler-artifact" and row.get("target", {}).get("name") == "shards_api" and row.get("executable"):
        path = Path(row["executable"])
        executables.append({"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
(evidence / "binaries.json").write_text(json.dumps(executables, indent=2))
if not executables:
    raise RuntimeError("No exact test executable reported by the actual compile")

results = {}
for name, test in [("borrowed", "borrowed::tests"), ("stream", "stream::tests")]:
    results[name] = command(name, ["cargo", "test", "--locked", "-j4", "--profile", "test-release", "-p", "shards-api", "--lib", test, "--", "--test-threads=1", "--nocapture"])
results["clippy"] = command("clippy", ["cargo", "clippy", "--locked", "-j4", "-p", "shards-api", "-p", "shards-afunix", "--all-targets", "--", "-D", "warnings"])
(evidence / "summary.json").write_text(json.dumps(results, indent=2))
print(json.dumps(results), flush=True)
sys.exit(0 if all(code == 0 for code in results.values()) else 1)
