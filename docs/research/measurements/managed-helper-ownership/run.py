#!/usr/bin/env python3
"""Finite private build and two actual helper ownership examples, without a VM."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys

directory, build_s, case_s, notification_s, target = sys.argv[1:]
root = Path(directory)
build_s, case_s, notification_s = map(int, (build_s, case_s, notification_s))
assert min(build_s, case_s, notification_s) > 0
assert notification_s * 4 < case_s
source = root / "source"
assert (source / "source-hashes.json").is_file()
for name, expected in json.loads((source / "source-hashes.json").read_text()).items():
    assert hashlib.sha256((source / name).read_bytes()).hexdigest() == expected, name
env = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
env.update(CARGO_HOME=str(root / "cargo-home"), CARGO_TARGET_DIR=str(root / "target"),
           RUSTUP_TOOLCHAIN="1.98.1", CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0")
assert target in ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl")
if target.endswith("-musl"):
    env.update(CC_x86_64_unknown_linux_musl=str(source / "scripts/zig-cc"),
               AR_x86_64_unknown_linux_musl=str(source / "scripts/zig-ar"),
               ZIG_TARGET="x86_64-linux-musl",
               ZIG_GLOBAL_CACHE_DIR=str(root / "zig-global-cache"),
               ZIG_LOCAL_CACHE_DIR=str(root / "zig-local-cache"))
# Restrict only this disposable driver and descendants to one already-allowed CPU.
os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
assert not Path(env["CARGO_TARGET_DIR"]).exists()
results = {"source_files": len(json.loads((source / "source-hashes.json").read_text())),
           "disk_before": shutil.disk_usage(root).free, "commands": {}}


def run(name, command, budget):
    (root / (name + ".command.json")).write_text(json.dumps(command) + "\n")
    with (root / (name + ".log")).open("wb") as log:
        p = subprocess.Popen(command, cwd=source, env=env, stdout=log,
                             stderr=subprocess.STDOUT, start_new_session=True)
        timeout = False
        try:
            code = p.wait(timeout=budget)
        except subprocess.TimeoutExpired:
            timeout = True
            # Own unreaped process-group leader pins this scope's group identity.
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            code = p.wait(timeout=case_s)
    results["commands"][name] = {"exit": code, "timed_out": timeout, "budget": budget}
    (root / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    print(name, json.dumps(results["commands"][name]), flush=True)
    return code


if run("build", ["cargo", "build", "--locked", "--release", "-j1", "--target", target,
                 "--message-format=json"], build_s):
    raise SystemExit("actual source build failed; native examples unrun")
binary = root / "target" / target / "release/managed-helper-ownership"
results["binary"] = {"path": str(binary), "bytes": binary.stat().st_size,
                     "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
for mode in ("death", "control-error"):
    run(mode, [sys.executable, str(root / "observe.py"), str(binary), str(root),
               mode, str(notification_s)], case_s)
if (source / "src/lib.rs").exists():
    run("strict-lib-clippy", ["cargo", "clippy", "--locked", "--release", "-j1", "--lib",
                              "--target", target, "--", "-D", "warnings"], build_s)
results["disk_after"] = shutil.disk_usage(root).free
(root / "results.json").write_text(json.dumps(results, indent=2) + "\n")
with (root / "storage.txt").open("w") as output:
    subprocess.run(["du", "-sk", str(root)], stdout=output, check=True)
print(json.dumps(results), flush=True)
