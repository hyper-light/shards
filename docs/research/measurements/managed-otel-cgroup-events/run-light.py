#!/usr/bin/env python3
"""Assigned pure cgroup event export checks. The profile executable is NEVER invoked."""

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
    record = {"commands": [], "environment": list(platform.uname()),
              "admission": {"cpus": 2, "memory_bytes": 2 * 1024**3,
                            "disk_bytes": 4 * 1024**3, "timeout_seconds": 600},
              "profiles_executed": False, "measurement": False, "owned_root": str(base), "fd_before": sorted(os.listdir("/proc/self/fd"))}
    deadline = time.monotonic() + 600
    env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    env.update(CARGO_HOME=str(base / "cargo-home"), CARGO_TARGET_DIR=str(base / "target"),
               TMPDIR=str(base / "tmp"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               RUSTFLAGS="-C debuginfo=0 -C codegen-units=1", RUSTUP_AUTO_INSTALL="0")
    for name in ("cargo-home", "target", "tmp"):
        (base / name).mkdir(exist_ok=False)
    record["build_environment"] = {key: env[key] for key in (
        "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR", "CARGO_BUILD_JOBS",
        "CARGO_INCREMENTAL", "RUSTFLAGS", "RUSTUP_AUTO_INSTALL")}
    wrapper = source / "crates/init/src/managed_otel_events_light.rs"
    with wrapper.open("xb") as generated:
        generated.write((source / "docs/research/measurements/managed-otel-cgroup-events/lib.rs").read_bytes())
    record["private_wrapper"] = digest(wrapper)
    before = {str(p.relative_to(source)): digest(p) for p in source.rglob("*") if p.is_file()}
    cargo = ["rustup", "run", "1.98.1", "cargo"]
    gnu, musl = "x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"
    root = pathlib.Path("docs/research/measurements")
    reader = str(root / "managed-otel-cgroup-events/Cargo.toml")
    encoder = str(root / "managed-otel-encoder/Cargo.toml")
    reference = str(root / "managed-otel-rust-reference/Cargo.toml")
    profile = str(root / "managed-otel/profile/Cargo.toml")

    def run(name, argv, input_path=None, output_path=None):
        command = {"name": name, "argv": argv, "cwd": str(source)}
        record["commands"].append(command)
        if input_path:
            command["stdin"] = {"path": str(input_path), **digest(input_path)}
        destination = output_path or out / (name + ".stdout")
        with destination.open("wb") as stdout, (out / (name + ".stderr")).open("wb") as stderr:
            input_stream = input_path.open("rb") if input_path else open(os.devnull, "rb")
            with input_stream as stdin:
                result = subprocess.run(argv, cwd=source, env=env, stdin=stdin, stdout=stdout,
                                        stderr=stderr, timeout=max(0, deadline - time.monotonic()), check=False)
        command.update(exit_code=result.returncode, stdout=str(destination))
        (out / (name + ".command.json")).write_text(json.dumps(command, indent=2) + "\n")
        print(json.dumps({"command": name, "exit_code": result.returncode}), flush=True)
        if result.returncode:
            raise RuntimeError(f"{name} returned {result.returncode}")

    def test_artifacts(build):
        artifacts = []
        for line in (out / (build + ".stdout")).read_text().splitlines():
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact" and message.get("executable") and message.get("profile", {}).get("test"):
                path = pathlib.Path(message["executable"])
                artifacts.append({"path": str(path), **digest(path)})
        return artifacts

    code = 1
    try:
        run("toolchains", ["rustup", "toolchain", "list"])
        if not any(line.split()[0].startswith("1.98.1-") for line in (out / "toolchains.stdout").read_text().splitlines() if line.split()):
            raise RuntimeError("existing 1.98.1 unavailable; no compiler installed")
        for name, argv in [("rustc-version", ["rustup", "run", "1.98.1", "rustc", "-Vv"]),
                           ("cargo-version", cargo + ["-V"]),
                           ("rustfmt-version", ["rustup", "run", "1.98.1", "rustfmt", "--version"]),
                           ("clippy-version", cargo + ["clippy", "--version"])]:
            run(name, argv)
        run("installed-targets", ["rustup", "target", "list", "--installed", "--toolchain", "1.98.1"])
        if not {gnu, musl} <= set((out / "installed-targets.stdout").read_text().splitlines()):
            raise RuntimeError("required GNU/musl standard libraries unavailable; no installation attempted")
        rust_files = ["crates/init/src/metrics.rs", str(root / "managed-otel-encoder/fixture.rs"),
                      str(root / "managed-otel-rust-reference/src/main.rs"),
                      str(root / "managed-otel-rust-reference/tests/equivalence.rs")]
        fmt = ["rustup", "run", "1.98.1", "rustfmt", "--edition", "2024"]
        run("format", fmt + rust_files)
        run("format-check", fmt + ["--check"] + rust_files)
        run("profile-format-check-only", fmt + ["--check", str(root / "managed-otel/profile/src/main.rs")])
        allowed = set(rust_files) | {"crates/init/src/metrics/tests.rs", "crates/init/src/metrics/otlp.rs", "crates/init/src/metrics/events.rs",
                                    str(root / "managed-otel-rust-reference/src/reference.rs")}
        for name, identity in before.items():
            if name not in allowed and digest(source / name) != identity:
                raise RuntimeError(f"formatter changed unowned source: {name}")
        record["test_artifacts"] = []
        for target in (gnu, musl):
            build = "reader-build-" + target
            run(build, cargo + ["test", "--locked", "--manifest-path", reader, "--lib", "--no-run",
                                "--message-format=json", "--target", target, "-j2"])
            artifacts = test_artifacts(build)
            if len(artifacts) != 1:
                raise RuntimeError("expected one actual Reader+codec test executable")
            record["test_artifacts"].extend(artifacts)
            run("reader-codec-tests-" + target, [artifacts[0]["path"], "metrics::", "--test-threads=1"])
            run("reader-codec-clippy-" + target, cargo + ["clippy", "--locked", "--manifest-path", reader,
                "--all-targets", "--target", target, "-j2", "--", "-D", "warnings"])
        run("encoder-build", cargo + ["build", "--locked", "--manifest-path", encoder, "--bin", "fixture", "--target", gnu, "-j2"])
        binary = base / "target" / gnu / "debug/fixture"
        record["encoder_artifact"] = {"path": str(binary), **digest(binary)}
        run("encoder-payload", [str(binary)])
        run("encoder-shape", ["python3", str(root / "managed-otel-encoder/check.py")], out / "encoder-payload.stdout")
        run("encoder-clippy", cargo + ["clippy", "--locked", "--manifest-path", encoder, "--all-targets", "--target", gnu, "-j2", "--", "-D", "warnings"])
        run("reference-build-tests", cargo + ["test", "--locked", "--manifest-path", reference, "--all-targets", "--no-run", "--message-format=json", "--target", gnu, "-j2"])
        artifacts = test_artifacts("reference-build-tests")
        if len(artifacts) != 2:
            raise RuntimeError("expected reference unit and complete equivalence executables")
        record["test_artifacts"].extend(artifacts)
        for at, artifact in enumerate(artifacts):
            run("reference-tests-" + str(at), [artifact["path"], "--test-threads=1"])
        run("reference-clippy", cargo + ["clippy", "--locked", "--manifest-path", reference, "--all-targets", "--target", gnu, "-j2", "--", "-D", "warnings"])
        binary = base / "target" / gnu / "debug/shards-managed-otel-rust-reference"
        record["reference_artifact"] = {"path": str(binary), **digest(binary)}
        payloads = out / "payloads"; payloads.mkdir(exist_ok=False)
        for mode in ("native-stream", "official-compact-buffer", "official-pretty-buffer"):
            run("payload-" + mode, [str(binary), mode], output_path=payloads / (mode + ".json"))
        run("complete-semantic-shape", ["python3", str(root / "managed-otel-rust-reference/check.py"), str(payloads)])
        # Compile/check only: no profile executable is built for invocation or run.
        run("profile-check-only", cargo + ["check", "--locked", "--manifest-path", profile, "--target", gnu, "-j2"])
        run("profile-alloc-feature-check-only", cargo + ["check", "--locked", "--manifest-path", profile, "--features", "alloc-count", "--target", gnu, "-j2"])
        record["test_artifacts_after"] = [{"path": a["path"], **digest(pathlib.Path(a["path"]))} for a in record["test_artifacts"]]
        if record["test_artifacts_after"] != record["test_artifacts"]:
            raise RuntimeError("test executable changed after execution")
        remaining = [str(p.relative_to(base / "tmp")) for p in (base / "tmp").rglob("*") if p.is_file() or p.is_symlink()]
        record["private_tmp_remaining_files"] = remaining
        if remaining:
            raise RuntimeError("private fixture files remained")
        code = 0
    except Exception as error:
        record["failure"] = {"type": type(error).__name__, "message": str(error)}
        if record["commands"]:
            name = record["commands"][-1]["name"]
            record["failure_logs"] = {kind: (out / (name + "." + kind)).read_text(errors="replace")
                                      for kind in ("stdout", "stderr") if (out / (name + "." + kind)).is_file()}
    finally:
        record["outcome"] = code
        record["fd_after"] = sorted(os.listdir("/proc/self/fd"))
        for name in ["crates/init/src/metrics.rs", "crates/init/src/metrics/tests.rs",
                     "crates/init/src/metrics/otlp.rs", "crates/init/src/metrics/events.rs", str(root / "managed-otel-encoder/fixture.rs"),
                     str(root / "managed-otel-rust-reference/src/main.rs"),
                     str(root / "managed-otel-rust-reference/src/reference.rs"),
                     str(root / "managed-otel-rust-reference/tests/equivalence.rs"),
                     str(root / "managed-otel/profile/src/main.rs")]:
            path = out / "formatted" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / name, path)
        record["source_files_after_format"] = {str(p.relative_to(source)): digest(p) for p in sorted(source.rglob("*")) if p.is_file()}
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
