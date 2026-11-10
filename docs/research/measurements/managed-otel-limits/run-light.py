#!/usr/bin/env python3
"""Assigned regular-file Reader checks only; no live cgroup or measurement."""

import hashlib
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tarfile
import time


def digest(path):
    data = path.read_bytes()
    return {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def main():
    base = pathlib.Path.cwd()
    source, out = base / "source", base / "out"
    out.mkdir(exist_ok=False)
    record = {"admission": {"cpus": 2, "memory_bytes": 2 * 1024**3,
                             "disk_bytes": 4 * 1024**3, "timeout_seconds": 600},
              "owned_root": str(base), "commands": [], "measurement": False,
              "environment": {"platform": platform.platform(), "uname": list(platform.uname())}}
    leaf = pathlib.Path("docs/research/measurements/managed-otel-limits")
    wrapper = pathlib.Path("crates/init/src/managed_otel_limits_light.rs")
    with (source / wrapper).open("xb") as output:
        output.write((source / leaf / "lib.rs").read_bytes())
    record["private_generated_crate_root"] = {"path": str(wrapper), **digest(source / wrapper)}
    before = {str(path.relative_to(source)): digest(path) for path in source.rglob("*") if path.is_file()}
    deadline = time.monotonic() + 600
    env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    env.update(CARGO_HOME=str(base / "cargo-home"), CARGO_TARGET_DIR=str(base / "target"),
               TMPDIR=str(base / "tmp"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               RUSTFLAGS="-C debuginfo=0 -C codegen-units=1", RUSTUP_AUTO_INSTALL="0")
    for name in ("cargo-home", "target", "tmp"):
        (base / name).mkdir(exist_ok=False)
    record["build_environment"] = {key: env[key] for key in (
        "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL",
        "RUSTFLAGS", "RUSTUP_AUTO_INSTALL")}
    code = 1

    def run(name, args):
        command = {"name": name, "argv": args, "cwd": str(source)}
        record["commands"].append(command)
        with (out / (name + ".stdout")).open("wb") as stdout, (out / (name + ".stderr")).open("wb") as stderr:
            result = subprocess.run(args, cwd=source, env=env, stdout=stdout, stderr=stderr,
                                    timeout=max(0, deadline - time.monotonic()), check=False)
        command["exit_code"] = result.returncode
        (out / (name + ".command.json")).write_text(json.dumps(command, indent=2) + "\n")
        print(json.dumps({"command": name, "exit_code": result.returncode}), flush=True)
        if result.returncode:
            raise RuntimeError(f"{name} returned {result.returncode}")

    try:
        run("toolchains", ["rustup", "toolchain", "list"])
        installed = (out / "toolchains.stdout").read_text()
        if not any(line.split()[0].startswith("1.98.1-") for line in installed.splitlines() if line.split()):
            raise RuntimeError("existing light compiler 1.98.1 unavailable; no installation attempted")
        cargo = ["rustup", "run", "1.98.1", "cargo"]
        run("rustc-version", ["rustup", "run", "1.98.1", "rustc", "-Vv"])
        run("cargo-version", cargo + ["-V"])
        run("rustfmt-version", ["rustup", "run", "1.98.1", "rustfmt", "--version"])
        run("clippy-version", cargo + ["clippy", "--version"])
        manifest = str(leaf / "Cargo.toml")
        run("format", cargo + ["fmt", "--manifest-path", manifest])
        allowed = {"crates/init/src/metrics.rs", "crates/init/src/metrics/tests.rs", str(wrapper)}
        for name, identity in before.items():
            if name not in allowed and digest(source / name) != identity:
                raise RuntimeError(f"formatter changed an unowned shared source: {name}")
        for name in ("crates/init/src/metrics.rs", "crates/init/src/metrics/tests.rs", str(leaf / "lib.rs")):
            destination = out / "formatted" / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / name, destination)
        run("format-check", cargo + ["fmt", "--manifest-path", manifest, "--", "--check"])
        run("build", cargo + ["test", "--manifest-path", manifest, "--lib", "--no-run", "--message-format=json", "-j2"])
        shutil.copyfile(source / leaf / "Cargo.lock", out / "Cargo.lock")
        artifacts = []
        for line in (out / "build.stdout").read_text().splitlines():
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact" and message.get("executable") and message.get("profile", {}).get("test"):
                executable = pathlib.Path(message["executable"])
                artifacts.append({"path": str(executable), **digest(executable)})
        record["test_artifacts"] = artifacts
        if len(artifacts) != 1:
            raise RuntimeError("expected one exact Reader harness test executable")
        run("reader-tests", [artifacts[0]["path"], "metrics::tests::", "--test-threads=1"])
        run("clippy", cargo + ["clippy", "--locked", "--manifest-path", manifest, "--all-targets", "-j2", "--", "-D", "warnings"])
        record["test_artifact_after"] = digest(pathlib.Path(artifacts[0]["path"]))
        if record["test_artifact_after"] != {key: artifacts[0][key] for key in ("bytes", "sha256")}:
            raise RuntimeError("test executable changed after execution")
        leftovers = [str(path.relative_to(base / "tmp")) for path in (base / "tmp").rglob("*") if path.is_file() or path.is_symlink()]
        record["private_tmp_remaining_files"] = leftovers
        if leftovers:
            raise RuntimeError("private regular-file fixture data remained")
        code = 0
    except Exception as error:
        record["failure"] = {"type": type(error).__name__, "message": str(error)}
        if record["commands"]:
            failed = record["commands"][-1]["name"]
            record["failure_logs"] = {
                stream: (out / (failed + "." + stream)).read_text(errors="replace")
                for stream in ("stdout", "stderr")
                if (out / (failed + "." + stream)).is_file()
            }
    finally:
        record["outcome"] = code
        record["source_files_after_format"] = {
            str(path.relative_to(source)): digest(path) for path in sorted(source.rglob("*")) if path.is_file()
        }
        (out / "result.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
        members = {str(path.relative_to(out)): digest(path) for path in sorted(out.rglob("*")) if path.is_file()}
        (out / "members.json").write_text(json.dumps(members, indent=2, sort_keys=True) + "\n")
        archive = base / "raw.tar.gz"
        with tarfile.open(archive, "w:gz") as packed:
            for path in sorted(out.rglob("*")):
                if path.is_file():
                    packed.add(path, arcname=str(path.relative_to(out)), recursive=False)
        print(json.dumps({"result": record, "archive": {"path": str(archive), **digest(archive)}}), flush=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
