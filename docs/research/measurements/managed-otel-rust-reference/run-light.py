#!/usr/bin/env python3
"""Run ONLY the assigned pure reference checks inside a new private source tree."""

import base64
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
import tomllib


def digest(path):
    data = path.read_bytes()
    return {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def main():
    base = pathlib.Path.cwd()
    source = base / "source"
    out = base / "out"
    out.mkdir(exist_ok=False)
    record = {"admission": {"cpus": 2, "memory_bytes": 2 * 1024**3,
                             "disk_bytes": 4 * 1024**3, "timeout_seconds": 600},
              "owned_root": str(base), "commands": [], "measurement": False,
              "environment": {"platform": platform.platform(), "uname": list(platform.uname()),
                              "disk_free_bytes_at_entry": shutil.disk_usage(base).free}}
    code = 1
    deadline = time.monotonic() + 600
    env = dict(os.environ)
    for name in list(env):
        if name.startswith("DYLD_"):
            env.pop(name)
    env.update(CARGO_HOME=str(base / "cargo-home"), CARGO_TARGET_DIR=str(base / "target"),
               TMPDIR=str(base / "tmp"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               RUSTFLAGS="-C debuginfo=0 -C codegen-units=1", RUSTUP_AUTO_INSTALL="0")
    for name in ("cargo-home", "target", "tmp"):
        (base / name).mkdir(exist_ok=False)
    record["build_environment"] = {key: env[key] for key in (
        "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL",
        "RUSTFLAGS", "RUSTUP_AUTO_INSTALL")}
    leaf = pathlib.Path("docs/research/measurements/managed-otel-rust-reference")

    def run(name, args, stdout=None):
        command = {"name": name, "argv": args, "cwd": str(source)}
        record["commands"].append(command)
        destination = out / (stdout or (name + ".stdout"))
        with destination.open("wb") as output, (out / (name + ".stderr")).open("wb") as error:
            result = subprocess.run(args, cwd=source, env=env, stdout=output, stderr=error,
                                    timeout=max(0, deadline - time.monotonic()), check=False)
        command["exit_code"] = result.returncode
        (out / (name + ".command.json")).write_text(json.dumps(command, indent=2) + "\n")
        print(json.dumps({"command": name, "exit_code": result.returncode}), flush=True)
        if result.returncode:
            raise RuntimeError(f"{name} returned {result.returncode}")
        return destination

    try:
        run("toolchains", ["rustup", "toolchain", "list"])
        installed = (out / "toolchains.stdout").read_text()
        if not any(line.split()[0].startswith("1.98.1-") for line in installed.splitlines() if line.split()):
            raise RuntimeError("required existing light compiler 1.98.1 is unavailable; no installation attempted")
        cargo = ["rustup", "run", "1.98.1", "cargo"]
        run("rustc-version", ["rustup", "run", "1.98.1", "rustc", "-Vv"])
        run("cargo-version", cargo + ["-V"])
        run("rustfmt-version", ["rustup", "run", "1.98.1", "rustfmt", "--version"])
        run("clippy-version", cargo + ["clippy", "--version"])
        manifest = str(leaf / "Cargo.toml")
        # Format only the new fixture sources. Preserve their actual formatter
        # output and hashes in the receipt; ROOT's shared source is not formatted.
        own_rust = [str(leaf / name) for name in ("src/main.rs", "src/reference.rs", "tests/equivalence.rs")]
        run("format-own", ["rustup", "run", "1.98.1", "rustfmt", "--edition", "2024"] + own_rust)
        for name in ("src/main.rs", "src/reference.rs", "tests/equivalence.rs"):
            target = out / "formatted" / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / leaf / name, target)
        run("format-check", cargo + ["fmt", "--manifest-path", manifest, "--", "--check"])
        run("build", cargo + ["build", "--manifest-path", manifest, "--message-format=json", "-j2"])
        lock = source / leaf / "Cargo.lock"
        shutil.copyfile(lock, out / "Cargo.lock")
        run("feature-tree", cargo + ["tree", "--locked", "--manifest-path", manifest, "-e", "features"])
        run("test", cargo + ["test", "--locked", "--manifest-path", manifest, "-j2"])
        run("clippy", cargo + ["clippy", "--locked", "--manifest-path", manifest, "--all-targets", "-j2", "--", "-D", "warnings"])
        executable = base / "target/debug/shards-managed-otel-rust-reference"
        record["binary"] = {"path": str(executable), **digest(executable)}
        for mode in ("native-stream", "official-compact-buffer", "official-pretty-buffer"):
            run(mode, [str(executable), mode], stdout=mode + ".json")
        run("schema-check", [sys.executable, str(leaf / "check.py"), str(out)])
        crates = []
        for vcs in (base / "cargo-home/registry/src").glob("*/opentelemetry-proto-0.33.1/.cargo_vcs_info.json"):
            crate = vcs.parent
            data = json.loads(vcs.read_text())
            if data["git"]["sha1"] != "372ebbc74d4dba71bb9820732f57f7316b4e7a1e":
                raise RuntimeError("registry proto source commit differs from required pin")
            files = {}
            for name in (".cargo_vcs_info.json", "Cargo.toml", "Cargo.toml.orig", "src/proto.rs",
                         "src/proto/tonic/opentelemetry.proto.metrics.v1.rs",
                         "src/proto/tonic/opentelemetry.proto.collector.metrics.v1.rs",
                         "src/proto/tonic/opentelemetry.proto.common.v1.rs",
                         "src/proto/tonic/opentelemetry.proto.resource.v1.rs"):
                files[name] = digest(crate / name)
            crates.append({"package": "opentelemetry-proto", "version": "0.33.1", "vcs": data, "files": files})
        if len(crates) != 1:
            raise RuntimeError("expected exactly one private pinned proto registry source")
        record["registry_provenance"] = crates
        code = 0
    except Exception as error:
        record["failure"] = {"type": type(error).__name__, "message": str(error)}
    finally:
        record["outcome"] = code
        record["source_files_after_format"] = {
            str(path.relative_to(source)): digest(path)
            for path in sorted(source.rglob("*")) if path.is_file()
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
