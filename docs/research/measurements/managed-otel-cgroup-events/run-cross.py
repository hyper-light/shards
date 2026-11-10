#!/usr/bin/env python3
"""Assigned Mac-to-Linux static checks of frozen source; never run project binaries."""

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
    record = {
        "source_commit": "67a0cdadee0479c0998041b84f77df344730d24d",
        "policy_main": "ca3ab10c151aa5539e757f1cc76029746ccbb561",
        "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
        "commands": [], "environment": list(platform.uname()), "owned_root": str(base),
        "envelope": {"cpus": 2, "memory_bytes": 2 * 1024**3, "disk_bytes": 4 * 1024**3,
                     "timeout_s": 600, "cargo_jobs": 2},
        "native_linux_tests": "UNRUN", "official_reference_execution": "UNRUN",
        "project_executables_executed": False, "profiles_executed": False,
        "source_archive": digest(base / "source.tar.gz"), "driver": digest(pathlib.Path(__file__)),
    }
    deadline = time.monotonic() + 600
    floor = 30 * 1024**3
    env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    env.update(CARGO_HOME=str(base / "cargo-home"), CARGO_TARGET_DIR=str(base / "target"),
               TMPDIR=str(base / "tmp"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               RUSTFLAGS="-C debuginfo=0 -C codegen-units=1", RUSTUP_AUTO_INSTALL="0")
    record["build_environment"] = {key: env[key] for key in (
        "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR", "CARGO_BUILD_JOBS",
        "CARGO_INCREMENTAL", "RUSTFLAGS", "RUSTUP_AUTO_INSTALL")}
    before = {str(p.relative_to(source)): digest(p) for p in sorted(source.rglob("*")) if p.is_file()}

    def run(name, argv):
        free = shutil.disk_usage(base).free
        if free < floor:
            raise RuntimeError("Mac below 30GiB floor; no next command started")
        command = {"name": name, "argv": argv, "cwd": str(source), "free_before_bytes": free}
        record["commands"].append(command)
        with (out / (name + ".stdout")).open("wb") as stdout, (out / (name + ".stderr")).open("wb") as stderr:
            result = subprocess.run(argv, cwd=source, env=env, stdin=subprocess.DEVNULL,
                                    stdout=stdout, stderr=stderr,
                                    timeout=max(0, deadline - time.monotonic()), check=False)
        command.update(exit_code=result.returncode, free_after_bytes=shutil.disk_usage(base).free)
        (out / (name + ".command.json")).write_text(json.dumps(command, indent=2) + "\n")
        print(json.dumps({"command": name, "exit_code": result.returncode}), flush=True)
        if result.returncode:
            raise RuntimeError(f"{name} returned {result.returncode}")

    code = 1
    try:
        record["free_before_bytes"] = shutil.disk_usage(base).free
        if platform.system() != "Darwin" or platform.machine() != "arm64":
            raise RuntimeError("assigned ARM Mac host required")
        # Keep the complete admitted disk allowance above the owner's floor.
        if record["free_before_bytes"] < floor + record["envelope"]["disk_bytes"]:
            raise RuntimeError("less than 30GiB floor plus admitted 4GiB disk allowance")
        for name in ("cargo-home", "target", "tmp"):
            (base / name).mkdir(exist_ok=False)
        wrapper = source / "crates/init/src/managed_otel_events_light.rs"
        template = source / "docs/research/measurements/managed-otel-cgroup-events/lib.rs"
        with wrapper.open("xb") as generated:
            generated.write(template.read_bytes())
        record["private_generated_wrapper"] = {"path": str(wrapper), **digest(wrapper)}
        cargo = ["rustup", "run", "1.98.1", "cargo"]
        run("toolchains", ["rustup", "toolchain", "list"])
        if not any(line.split()[0].startswith("1.98.1-") for line in (out / "toolchains.stdout").read_text().splitlines() if line.split()):
            raise RuntimeError("existing 1.98.1 unavailable; no compiler installed")
        for name, argv in [("rustc-version", ["rustup", "run", "1.98.1", "rustc", "-Vv"]),
                           ("cargo-version", cargo + ["-V"]),
                           ("rustfmt-version", ["rustup", "run", "1.98.1", "rustfmt", "--version"]),
                           ("clippy-version", cargo + ["clippy", "--version"])]:
            run(name, argv)
        targets = ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl")
        run("installed-targets", ["rustup", "target", "list", "--installed", "--toolchain", "1.98.1"])
        if not set(targets) <= set((out / "installed-targets.stdout").read_text().splitlines()):
            raise RuntimeError("required existing GNU/musl libraries unavailable; no installation attempted")
        root = pathlib.Path("docs/research/measurements")
        run("format-check-only", ["rustup", "run", "1.98.1", "rustfmt", "--edition", "2024", "--check",
            "crates/init/src/metrics.rs", str(root / "managed-otel-encoder/fixture.rs"),
            str(root / "managed-otel-rust-reference/src/main.rs"),
            str(root / "managed-otel-rust-reference/tests/equivalence.rs"),
            str(root / "managed-otel/profile/src/main.rs")])
        reader = str(root / "managed-otel-cgroup-events/Cargo.toml")
        for target in targets:
            run("reader-codec-strict-" + target, cargo + ["clippy", "--locked", "--manifest-path", reader,
                "--all-targets", "--target", target, "-j2", "--", "-D", "warnings"])
        gnu = targets[0]
        for name, manifest in [
            ("encoder", root / "managed-otel-encoder/Cargo.toml"),
            ("official-reference", root / "managed-otel-rust-reference/Cargo.toml"),
        ]:
            run(name + "-cross-strict", cargo + ["clippy", "--locked", "--manifest-path", str(manifest),
                "--all-targets", "--target", gnu, "-j2", "--", "-D", "warnings"])
        profile = str(root / "managed-otel/profile/Cargo.toml")
        run("profile-default-cross-check-only", cargo + ["check", "--locked", "--manifest-path", profile,
                                                        "--target", gnu, "-j2"])
        run("profile-feature-cross-check-only", cargo + ["check", "--locked", "--manifest-path", profile,
            "--features", "alloc-count", "--target", gnu, "-j2"])
        after = {name: digest(source / name) for name in before}
        record["original_source_files_unchanged"] = after == before
        if after != before:
            raise RuntimeError("frozen source changed during static checks")
        code = 0
    except Exception as error:
        record["failure"] = {"type": type(error).__name__, "message": str(error)}
        if record["commands"]:
            name = record["commands"][-1]["name"]
            record["failure_logs"] = {kind: (out / (name + "." + kind)).read_text(errors="replace")
                                      for kind in ("stdout", "stderr") if (out / (name + "." + kind)).is_file()}
    finally:
        record["outcome"] = code
        record["free_after_bytes"] = shutil.disk_usage(base).free
        record["source_files_after"] = {str(p.relative_to(source)): digest(p) for p in sorted(source.rglob("*")) if p.is_file()}
        if record["free_after_bytes"] < floor:
            record["floor_violation"] = True
            code = record["outcome"] = 1
        (out / "result.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
        members = {str(p.relative_to(out)): digest(p) for p in sorted(out.rglob("*")) if p.is_file()}
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
