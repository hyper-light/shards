#!/usr/bin/env python3
"""Scoped light checks; no VM, allocation injection, profiling or mutation."""

import hashlib
import json
import os
import pathlib
import subprocess
import sys


root = pathlib.Path.cwd()
assert root.name.startswith("main-cmdline-error-type-"), root
evidence = pathlib.Path.home() / "main-cmdline-error-type-evidence" / "v3"
target = pathlib.Path.home() / "main-cmdline-error-type-target"
prior = pathlib.Path.home() / "main-kernel-argv-target"
assert not evidence.exists() and target.is_dir()
evidence.mkdir()
(evidence / "identity.json").write_text(json.dumps({
    "cwd": str(root),
    "source": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "status": subprocess.check_output(["git", "status", "--short"], text=True),
    "compiler": subprocess.check_output(["rustc", "-Vv"], text=True),
    "kernel": subprocess.check_output(["uname", "-a"], text=True),
    "envelope": {"cpus": 4, "memory_gib": 1, "disk_gib": 2},
}, indent=2) + "\n")
(evidence / "cache-source.txt").write_text(
    f"Read-only reflink source: {prior}\nPrivate writable target: {target}\n"
)
paths = [
    "crates/vmm/src/vm/mod.rs",
    "crates/vmm/src/vm/x86_64.rs",
    "crates/vmm/src/vm/runtime.rs",
    "crates/vmm/src/vm/unsupported.rs",
    "crates/shards/src/vm_run.rs",
]
(evidence / "source-sha256.json").write_text(json.dumps({
    p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in paths
}, indent=2) + "\n")
(evidence / "source.patch").write_bytes(subprocess.check_output(["git", "diff", "--", *paths]))
for path in paths:
    (root / path).touch()
env = os.environ.copy()
env["CARGO_TARGET_DIR"] = str(target)
env["SHARDS_HELPERS"] = "skip"
commands = [
    ("policy", ["scripts/check-development-policy"]),
    ("format", ["rustfmt", "--edition", "2024", "--check", "--config", "skip_children=true", "--config-path", "rustfmt.toml", *paths]),
]
for name, triple in [("gnu", "x86_64-unknown-linux-gnu"), ("musl", "x86_64-unknown-linux-musl")]:
    for case, unit_filter in [
        ("composer", "vm::cmdline::tests::"),
        ("typed", "vm::tests::a_command_line_start_error_preserves_the_cause_and_message"),
    ]:
        commands.append((f"{name}-{case}", [
            "cargo", "test", "--locked", "-j4", "-p", "shards-vmm", "--lib", "--target", triple,
            unit_filter, "--", "--nocapture", "--test-threads=1",
        ]))
for name, triple in [
    ("gnu", "x86_64-unknown-linux-gnu"),
    ("musl", "x86_64-unknown-linux-musl"),
    ("msvc", "x86_64-pc-windows-msvc"),
]:
    commands.append((f"{name}-clippy", [
        "cargo", "clippy", "--locked", "-j4", "-p", "shards-vmm", "-p", "shards-vm-process",
        "--all-targets", "--target", triple, "--", "-D", "warnings",
    ]))
for name, command in commands:
    (evidence / f"{name}.command.json").write_text(json.dumps({"argv": command, "cwd": str(root), "CARGO_TARGET_DIR": str(target), "SHARDS_HELPERS": "skip"}, indent=2) + "\n")
    with (evidence / f"{name}.log").open("wb") as log:
        result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
    (evidence / f"{name}.exit").write_text(f"{result.returncode}\n")
    print(f"{name}: {result.returncode}", flush=True)
    if result.returncode:
        print((evidence / f"{name}.log").read_text(), flush=True)
        sys.exit(result.returncode)
current = {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in paths}
assert current == json.loads((evidence / "source-sha256.json").read_text())
(evidence / "source-after-sha256.json").write_text(json.dumps(current, indent=2) + "\n")
print(subprocess.check_output(["du", "-sk", str(target)], text=True), end="")
