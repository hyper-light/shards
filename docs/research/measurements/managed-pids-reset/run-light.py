#!/usr/bin/env python3
"""Scoped actual resources/update-core units; not a complete daemon/VM gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--records", type=Path, required=True)
    parser.add_argument("--seconds", type=float, required=True)
    args = parser.parse_args()
    if args.seconds <= 0:
        parser.error("positive caller deadline required")
    root = Path(__file__).resolve().parents[4]
    if shutil.disk_usage(root).free < 30 * (1 << 30):
        raise SystemExit("Mac policy floor: hold builds below 30 GiB")
    args.directory.mkdir(parents=True, exist_ok=False)
    (args.directory / "src").mkdir()
    args.records.mkdir(parents=True, exist_ok=False)
    sources = ["crates/shards/src/resources.rs", "crates/shards/src/daemon.rs", "crates/shards/src/daemon/visit.rs", "crates/shards/src/daemon/update.rs", "Cargo.lock"]
    hashes = lambda: {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sources}
    before = hashes()
    update = (root / sources[3]).read_text()
    prefix, tail = update.split("impl<D: Disk> Daemon<D> {", 1)
    _, tail = tail.split("/// opts.ParseRestartPolicy", 1)
    update = prefix + "/// opts.ParseRestartPolicy" + tail
    for line in ["use super::commands::Reply;", "use super::{Daemon, REQUEST, RunState, lock};", "use crate::containers::Disk;"]:
        update = update.replace(line, "")
    (args.directory / "src/update.rs").write_text(update)
    (args.directory / "src/lib.rs").write_text(
        "#![allow(dead_code)]\n"
        f"#[path = {json.dumps(str(root / sources[0]))}]\nmod resources;\n"
        "mod update;\n"
    )
    manifest = ["[package]", 'name = "managed-pids-reset-light"', 'version = "0.0.0"', 'edition = "2024"', "[workspace]", "[dependencies]"]
    for package, directory in [("shards-ipc", "ipc"), ("shards-cmdline", "cmdline")]:
        manifest.append(f"{package} = {{ path = {json.dumps(str(root / 'crates' / directory))} }}")
    cargo = (root / "Cargo.toml").read_text()
    for section in ["rust", "clippy"]:
        body = cargo.split(f"[workspace.lints.{section}]\n", 1)[1].split("\n[", 1)[0]
        manifest.extend([f"[lints.{section}]", body])
    (args.directory / "Cargo.toml").write_text("\n".join(manifest) + "\n")
    (args.directory / "Cargo.lock").write_bytes((root / "Cargo.lock").read_bytes())
    (args.records / "source.json").write_text(json.dumps({
        "base": "4299285281a0b588ed04b854842c1c14b0950260", "source": before,
        "resources_module": "actual source by path, unchanged",
        "update_adapter": "only outer Daemon impl and its imports omitted; actual asked/merge/restart-policy/tests retained",
        "update_adapter_sha256": hashlib.sha256(update.encode()).hexdigest(),
        "not_proven": ["three complete daemon callers", "real cgroup/VM reset", "allocation fault paths", "seven target matrix"],
    }, indent=2) + "\n")
    environment = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
    commands = [
        ("units", ["cargo", "test", "--manifest-path", str(args.directory / "Cargo.toml"), "--lib", "-j2", "--", "--test-threads=1"]),
        ("clippy", ["cargo", "clippy", "--locked", "--manifest-path", str(args.directory / "Cargo.toml"), "--lib", "-j2", "--", "-D", "warnings"]),
    ]
    deadline = time.monotonic() + args.seconds
    results = []
    for name, command in commands:
        with (args.records / (name + ".stdout")).open("wb") as out, (args.records / (name + ".stderr")).open("wb") as err:
            try:
                done = subprocess.run(command, cwd=root, env=environment, stdout=out, stderr=err, timeout=max(0, deadline - time.monotonic()))
                status = done.returncode
            except subprocess.TimeoutExpired:
                status = "caller deadline"
        results.append({"name": name, "command": command, "status": status})
        if status != 0:
            break
    record = {"source_before": before, "source_after": hashes(), "checks": results}
    (args.records / "receipt.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2))
    raise SystemExit(0 if len(results) == len(commands) and all(r["status"] == 0 for r in results) and record["source_after"] == before else 1)


if __name__ == "__main__":
    main()
