#!/usr/bin/env python3
"""Remote private snapshot-type/shape units only; never the managed-output caller."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib


def main():
    revision = sys.argv[1]
    if re.fullmatch(r"[0-9a-f]{40}", revision) is None:
        raise SystemExit("full source revision required")
    source = Path(__file__).resolve().parent
    target = Path.home() / f"managed-allocation-metrics-target-startup-{revision}"
    records = Path.home() / f"managed-allocation-metrics-light-startup-{revision}"
    for path in (target, records):
        if path.exists():
            raise SystemExit(f"new private path required: {path}")
    target.mkdir()
    records.mkdir()
    scratch = target / "tmp"
    scratch.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    env.update({
        "CARGO_HOME": str(target / "cargo-home"),
        "CARGO_TARGET_DIR": str(target / "target"),
        "CARGO_BUILD_JOBS": "1",
        "CARGO_INCREMENTAL": "0",
        "RUSTC_WRAPPER": "",
        "RUSTC_WORKSPACE_WRAPPER": "",
        "RUSTFLAGS": "-C codegen-units=1 -C debuginfo=0",
        "TMPDIR": str(scratch),
    })
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("installed cargo rustup proxy required")
    rustc = subprocess.check_output(
        ["rustup", "which", "--toolchain", "1.98.1", "rustc"], text=True
    ).strip()
    env["RUSTC"] = rustc

    def write(name, value):
        (records / name).write_text(json.dumps(value, indent=2) + "\n")

    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    reference = json.loads((source / "source-reference.json").read_text())
    if reference["revision"] != revision:
        raise SystemExit("source revision mismatch")
    for item in reference["files"]:
        if sha(source / item["path"]) != item["sha256"]:
            raise SystemExit("private source digest mismatch")

    def check(name, argv):
        write(name + ".command.json", {"argv": argv, "cwd": str(source), "timeout_seconds": 120})
        with (records / (name + ".log")).open("wb") as stream:
            try:
                code = subprocess.run(
                    argv, cwd=source, env=env, stdout=stream,
                    stderr=subprocess.STDOUT, timeout=120,
                ).returncode
            except subprocess.TimeoutExpired:
                code = 124
        (records / (name + ".exit")).write_text(str(code) + "\n")
        return code

    write("scope.json", {
        "revision": revision,
        "admission": "1CPU/1GiB/2GiB; sync:false; serial private light only",
        "compiled": "byte-extracted StartupAllocations/artifact structs + existing allocation_snapshot_record + real managed_alloc/serde_json types",
        "not_compiled_or_run": "actual artifact closure/resolvers, full fixture/caller, native queries, VM, profiles, paired executor, campaigns",
        "allocator": "private unregistered MANAGED_ALLOC instance; no allocation measurements/reset/global registration",
        "source": str(source), "target": str(target), "records": str(records),
        "environment_overrides": {key: env[key] for key in (
            "CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL",
            "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS", "RUSTC", "TMPDIR",
        )},
    })
    (records / "rustc.txt").write_bytes(subprocess.check_output([rustc, "-Vv"], env=env))
    (records / "cargo.txt").write_bytes(subprocess.check_output([cargo, "+1.98.1", "-Vv"], env=env))
    write("tools.json", {
        "python": {"version": sys.version, "path": str(Path(sys.executable).resolve()), "sha256": sha(Path(sys.executable).resolve())},
        "rustc": {"path": rustc, "sha256": sha(Path(rustc))},
        "cargo_proxy": cargo,
    })
    shutil.copyfile(source / "source-reference.json", records / "source-reference.json")
    outcomes = {"build": check("build", [
        cargo, "+1.98.1", "test", "--lib", "--no-run", "--message-format=json", "-j", "1",
    ])}
    if outcomes["build"] == 0:
        artifacts = []
        for line in (records / "build.log").read_text().splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("reason") == "compiler-artifact" and event.get("profile", {}).get("test") is True and event.get("target", {}).get("name") == "managed_startup_shape" and event.get("executable"):
                artifacts.append(Path(event["executable"]))
        if len(artifacts) != 1:
            raise SystemExit("one private libtest executable required")
        binary = artifacts[0]
        write("binary.json", {"path": str(binary), "bytes": binary.stat().st_size, "sha256": sha(binary)})
        outcomes["shape-tests"] = check("shape-tests", [
            str(binary), "startup_shapes::", "--test-threads=1", "--nocapture",
        ])
        if outcomes["shape-tests"] == 0:
            outcomes["clippy"] = check("clippy", [
                cargo, "+1.98.1", "clippy", "--all-targets", "-j", "1", "--",
                "-D", "warnings", "-D", "clippy::undocumented_unsafe_blocks",
            ])
    if (source / "Cargo.lock").exists():
        shutil.copyfile(source / "Cargo.lock", records / "Cargo.lock")
        lock = tomllib.loads((source / "Cargo.lock").read_text())
        direct = [p for p in lock["package"] if p["name"] == "serde_json"]
        write("serde-json-identity.json", direct)
        if len(direct) != 1 or direct[0]["version"] != "1.0.151" or direct[0]["checksum"] != "c841b55ecdae098c80dcae9cf767f6f8a0c2cdb3416bbef72181df4d0fe73f14":
            outcomes["serde-json-source"] = 1
    write("outcomes.json", outcomes)
    write("root-owed.json", {"unrun": "full source-matching caller/helpers, original five artifact closure/borrow typecheck, real N1/startup records/early-panic failures, native/profile/bench/paired/guard/cleanup/seven-target/main gates"})
    write("MANIFEST.json", [{"path": p.name, "bytes": p.stat().st_size, "sha256": sha(p)} for p in sorted(records.iterdir()) if p.is_file()])
    print(json.dumps({"records": str(records), "outcomes": outcomes}))
    return 0 if set(outcomes) == {"build", "shape-tests", "clippy"} and all(code == 0 for code in outcomes.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
