#!/usr/bin/env python3
"""One admitted native read of this disposable process's own cgroup; no retry."""
import hashlib
import json
import os
import pathlib
import platform
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
    fixture = pathlib.Path("docs/research/measurements/managed-otel-limits-export/native-own-cgroup")
    record = {"commands": [], "environment": list(platform.uname()), "owned_root": str(base),
              "admission": {"cpus": 2, "memory_bytes": 2 * 1024**3, "disk_bytes": 4 * 1024**3,
                            "timeout_seconds": 600, "cargo_jobs": 2},
              "measurement": False, "profiles_executed": False, "native_invocations": 0}
    deadline = time.monotonic() + 600
    env = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
    for name in ("target", "cargo-home", "tmp"):
        (base / name).mkdir(exist_ok=False)
    env.update(CARGO_HOME=str(base / "cargo-home"), CARGO_TARGET_DIR=str(base / "target"),
               TMPDIR=str(base / "tmp"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               RUSTFLAGS="-C debuginfo=0 -C codegen-units=1", RUSTUP_AUTO_INSTALL="0")
    record["build_environment"] = {k: env[k] for k in ("CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR",
        "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "RUSTFLAGS", "RUSTUP_AUTO_INSTALL")}
    before = {str(p.relative_to(source)): digest(p) for p in source.rglob("*") if p.is_file()}
    cargo = ["rustup", "run", "1.98.1", "cargo"]
    target = "x86_64-unknown-linux-gnu"
    manifest = str(fixture / "Cargo.toml")

    def run(name, argv):
        entry = {"name": name, "argv": argv, "cwd": str(source)}
        record["commands"].append(entry)
        with (out / (name + ".stdout")).open("xb") as stdout, (out / (name + ".stderr")).open("xb") as stderr:
            result = subprocess.run(argv, cwd=source, env=env, stdin=subprocess.DEVNULL,
                stdout=stdout, stderr=stderr, timeout=max(0, deadline - time.monotonic()), check=False)
        entry["exit_code"] = result.returncode
        (out / (name + ".command.json")).write_text(json.dumps(entry, indent=2) + "\n")
        print(json.dumps({"command": name, "exit_code": result.returncode}), flush=True)
        if result.returncode:
            raise RuntimeError(f"{name} returned {result.returncode}; no retry or fallback")

    code = 1
    try:
        run("toolchains", ["rustup", "toolchain", "list"])
        if not any(line.split()[0].startswith("1.98.1-") for line in (out / "toolchains.stdout").read_text().splitlines() if line.split()):
            raise RuntimeError("installed toolchain unavailable; no installation")
        for name, argv in [("rustc-version", ["rustup", "run", "1.98.1", "rustc", "-Vv"]),
                           ("cargo-version", cargo + ["-V"]),
                           ("rustfmt-version", ["rustup", "run", "1.98.1", "rustfmt", "--version"]),
                           ("clippy-version", cargo + ["clippy", "--version"])]:
            run(name, argv)
        run("installed-targets", ["rustup", "target", "list", "--installed", "--toolchain", "1.98.1"])
        if target not in (out / "installed-targets.stdout").read_text().splitlines():
            raise RuntimeError("native GNU standard library unavailable; no installation")
        rust = [str(fixture / name) for name in ("lib.rs", "fixture.rs")]
        fmt = ["rustup", "run", "1.98.1", "rustfmt", "--edition", "2024", "--config", "skip_children=true"]
        run("format-owned-only", fmt + rust)
        run("format-check-owned-only", fmt + ["--check"] + rust)
        for name, identity in before.items():
            if name not in rust and digest(source / name) != identity:
                raise RuntimeError(f"formatter changed frozen input {name}")
        wrapper = source / "crates/init/src/managed_otel_native_own_cgroup.rs"
        with wrapper.open("xb") as generated:
            generated.write((source / fixture / "lib.rs").read_bytes())
        record["private_wrapper"] = {"path": str(wrapper.relative_to(source)), **digest(wrapper)}
        run("build", cargo + ["build", "--locked", "--manifest-path", manifest, "--bin", "fixture", "--target", target, "-j2"])
        run("build-resolution-tests", cargo + ["test", "--locked", "--manifest-path", manifest, "--bin", "fixture", "--no-run", "--message-format=json", "--target", target, "-j2"])
        tests = []
        for line in (out / "build-resolution-tests.stdout").read_text().splitlines():
            item = json.loads(line)
            if item.get("reason") == "compiler-artifact" and item.get("executable") and item.get("profile", {}).get("test"):
                tests.append({"path": item["executable"], **digest(pathlib.Path(item["executable"]))})
        if len(tests) != 1:
            raise RuntimeError("expected one fixture resolution test binary")
        record["test_artifacts"] = tests
        run("resolution-tests", [tests[0]["path"], "own_resolution_tests::", "--test-threads=1"])
        run("strict-clippy-bin", cargo + ["clippy", "--locked", "--manifest-path", manifest, "--bin", "fixture", "--target", target, "-j2", "--", "-D", "warnings"])
        binary = base / "target" / target / "debug/fixture"
        record["native_artifact_before"] = {"path": str(binary), **digest(binary)}
        native = out / "native"
        native.mkdir(exist_ok=False)
        # Explicit fixture-only identity inputs; no actual Shards run/VM is claimed.
        record["native_invocations"] += 1
        run("native-once", [str(binary), str(native), "0" * 63 + "1", "0" * 63 + "2", "0"])
        result = json.loads((native / "result.json").read_bytes())
        record["native_outcome"] = result
        if result["outcome"]["type"] != "Observed" or not result["fd_sets_equal"]:
            raise RuntimeError("native source or descriptor witness limited; no retry")
        record["native_artifact_after"] = {"path": str(binary), **digest(binary)}
        if record["native_artifact_before"] != record["native_artifact_after"]:
            raise RuntimeError("native binary identity changed")
        record["test_artifacts_after"] = [{"path": a["path"], **digest(pathlib.Path(a["path"]))} for a in tests]
        if record["test_artifacts_after"] != tests:
            raise RuntimeError("resolution test binary identity changed")
        remaining = [str(p.relative_to(base / "tmp")) for p in (base / "tmp").rglob("*") if p.is_file() or p.is_symlink()]
        record["private_tmp_remaining_files"] = remaining
        if remaining:
            raise RuntimeError("private temporary fixture files remained")
        code = 0
    except Exception as error:
        record["failure"] = {"type": type(error).__name__, "message": str(error)}
        if (out / "native/result.json").is_file():
            record["native_outcome"] = json.loads((out / "native/result.json").read_bytes())
        if record["commands"]:
            name = record["commands"][-1]["name"]
            record["failure_logs"] = {kind: (out / (name + "." + kind)).read_text(errors="replace")
                                      for kind in ("stdout", "stderr") if (out / (name + "." + kind)).is_file()}
    finally:
        record["outcome"] = code
        for name in ("lib.rs", "fixture.rs"):
            destination = out / "formatted" / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes((source / fixture / name).read_bytes())
        record["source_files_after_format"] = {str(p.relative_to(source)): digest(p) for p in sorted(source.rglob("*")) if p.is_file()}
        (out / "result.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
        members = {str(p.relative_to(out)): digest(p) for p in sorted(out.rglob("*")) if p.is_file()}
        (out / "members.json").write_text(json.dumps(members, indent=2, sort_keys=True) + "\n")
        raw = base / "raw.tar.gz"
        with tarfile.open(raw, "w:gz") as archive:
            for p in sorted(out.rglob("*")):
                if p.is_file():
                    archive.add(p, arcname=str(p.relative_to(out)), recursive=False)
        print(json.dumps({"result": record, "archive": {"path": str(raw), **digest(raw)}}), flush=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
