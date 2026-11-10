#!/usr/bin/env python3
"""One approved ordinary teardown case; no fault, allocator or performance actor."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time
import traceback

SELECTOR = "client::destroy_observation::ordinary_final_owner_releases_native_storage"
SOURCE = "79276d1923811bde0b275dda7afa98f587737f3f"


def sha(body):
    return hashlib.sha256(body).hexdigest()


def write_new(path, body):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with path.open("xb") as output:
        output.write(body)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    sys.dont_write_bytecode = True
    row = {"source_revision": SOURCE, "root": str(root), "selector": SELECTOR,
           "commands": [], "case": None, "native_case_attempts": 0,
           "controller_fds_before": len(os.listdir("/dev/fd")), "expected_ordinary_result": False}
    try:
        if platform.system() != "Darwin" or platform.machine() not in ["arm64", "aarch64"]:
            raise RuntimeError("approved native ARM64 Mac required")
        bundle = json.loads((root / "inputs/bundle.json").read_text())
        if bundle["source_revision"] != SOURCE or bundle["version"]["has_allocation_refusal"]:
            raise ValueError("not the reviewed ordinary closure")
        source = root / "source"
        env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
        env.update(CARGO_HOME=str(root / "cargo-home"), CARGO_TARGET_DIR=str(root / "target"),
                   CARGO_BUILD_JOBS="2", CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS="1",
                   RUSTFLAGS="-Cllvm-args=-threads=1")
        row["environment"] = {key: env[key] for key in ["CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS",
                                                       "CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS", "RUSTFLAGS"]}
        for name, item in bundle["version"]["files"].items():
            body = (source / name).read_bytes()
            if len(body) != item["bytes"] or sha(body) != item["sha256"]:
                raise ValueError("pre-build source mismatch: " + name)
        if sha((root / "held_exit.py").read_bytes()) != bundle["helper"]["sha256"]:
            raise ValueError("held supervisor identity mismatch")
        spec = importlib.util.spec_from_file_location("approved_held_exit", root / "held_exit.py")
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        row["free_bytes_before_build"] = shutil.disk_usage(root).free
        if row["free_bytes_before_build"] < (30 + 4) * 1024 ** 3:
            raise RuntimeError("30GiB floor plus complete admitted 4GiB envelope unavailable")
        row["os"] = subprocess.check_output(["sw_vers"], text=True)
        row["uname"] = list(platform.uname())
        for tool in ["cargo", "rustc", "clippy-driver"]:
            # rustup which refuses a missing toolchain, rather than installing it.
            row[tool + "_path"] = subprocess.check_output(
                ["rustup", "which", "--toolchain", "1.98.1", tool], env=env, text=True).strip()
        row["rustc"] = subprocess.check_output([row["rustc_path"], "-vV"], env=env, text=True)
        write_new(root / "raw/before-build.json", (json.dumps(row, indent=2) + "\n").encode())
        # All three Rust build/lint stages share this one original compile budget.
        compile_deadline = time.monotonic() + 180
        cargo = ["cargo", "+1.98.1"]
        stages = [
            ("strict-production", cargo + ["clippy", "--locked", "-j2", "--profile", "test-release", "-p", "shards-api", "--lib", "--", "-D", "warnings"]),
            ("compile", cargo + ["test", "--locked", "-j2", "--profile", "test-release", "-p", "shards-api", "--lib", "--no-run", "--message-format=json"]),
            ("strict-test", cargo + ["clippy", "--locked", "-j2", "--profile", "test-release", "-p", "shards-api", "--lib", "--tests", "--", "-D", "warnings"]),
        ]
        for label, argv in stages:
            if shutil.disk_usage(root).free < 30 * 1024 ** 3:
                raise RuntimeError("local floor lost; no further build")
            remaining = compile_deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("original aggregate compile deadline expired")
            result = helper.command(argv, source, env, root / ("raw/" + label), remaining)
            row["commands"].append({"stage": label, **result})
            print(json.dumps({"stage": label, "exit": result["exit"]}), flush=True)
            if result["exit"] != 0:
                raise RuntimeError(label + " failed before any native case")
        artifacts = [json.loads(line) for line in (root / "raw/compile.stdout").read_text().splitlines()
                     if line.startswith("{")]
        binaries = [Path(item["executable"]) for item in artifacts if item.get("reason") == "compiler-artifact"
                    and item.get("executable") and item.get("target", {}).get("name") == "shards_api"]
        if len(binaries) != 1:
            raise RuntimeError("expected one actual API lib-test artifact")
        binary = binaries[0]
        frozen = root / "frozen/api-final-owner"
        body = binary.read_bytes()
        write_new(frozen, body)
        frozen.chmod(0o700)
        if sha(frozen.read_bytes()) != sha(body):
            raise ValueError("frozen binary mismatch")
        row["binary"] = {"built_path": str(binary), "frozen_path": str(frozen), "sha256": sha(body)}
        selected = helper.command([str(frozen), SELECTOR, "--exact", "--ignored", "--list"],
                                  source, env, root / "raw/selection", 15)
        row["selection"] = selected
        listing = (root / "raw/selection.stdout").read_text()
        if selected["exit"] != 0 or listing.count(SELECTOR + ": test") != 1 or "1 test," not in listing:
            raise RuntimeError("exact ignored selector did not select one test")
        row["native_case_attempts"] = 1
        result = helper.case(frozen, root / "raw/ordinary", 15, False, env, selector=SELECTOR)
        row["case"] = result
        output = (root / "raw/ordinary/stdout").read_text()
        events = [line.removeprefix("TEARDOWN_EVENT ") for line in output.splitlines()
                  if line.startswith("TEARDOWN_EVENT ")]
        expected = ["SharedDropEntered"]
        for object_name in ["State", "Changed", "Tail", "Drained"]:
            expected += [f"DestroyEntered({object_name})",
                         f"DestroyReturned {{ object: {object_name}, code: 0 }}",
                         f"PointeeDropReturned({object_name})"]
        expected.append("OriginalSharedFieldsDropped")
        row["events"] = events
        row["exact_derived_events_match"] = events == expected
        row["actual_parent_echild_marker"] = "COLD_NATIVE_REAP_ECHILD\n" in output
        row["eight_actual_join_marker"] = "TEARDOWN_JOINED connectors=2 waiters=2 stopper=1 muxer=1 api_drain=1 api_watch=1\n" in output
        row["field_and_channel_marker"] = "TEARDOWN_POINTEES_DROPPED_ORIGINAL_FIELDS_DROPPED_OBSERVERS_CLOSED\n" in output
        row["actual_one_pass_no_ignore"] = "1 passed; 0 failed; 0 ignored;" in output
        row["vsock_directory_absent"] = bool(result.get("vsock")) and not Path(result["vsock"]).parent.exists()
        row["expected_ordinary_result"] = (result["exit"] == 0 and not result["timed_out"]
            and not result.get("driver_error") and result["observer_registered_before_release"]
            and result["standin_exit_observed"] and result["test_exit_observed"]
            and all(row[key] for key in ["exact_derived_events_match", "actual_parent_echild_marker",
                       "eight_actual_join_marker", "field_and_channel_marker", "actual_one_pass_no_ignore", "vsock_directory_absent"]))
    except Exception as error:
        row["error"] = repr(error)
        row["traceback"] = traceback.format_exc()
    finally:
        row["controller_fds_after"] = len(os.listdir("/dev/fd"))
        row["free_bytes_after"] = shutil.disk_usage(root).free
        row["controller_fds_returned"] = row["controller_fds_before"] == row["controller_fds_after"]
        row["expected_ordinary_result"] &= row["controller_fds_returned"]
        write_new(root / "raw/summary.json", (json.dumps(row, indent=2) + "\n").encode())
        print(json.dumps({"root": str(root), "commands": row["commands"], "native_case_attempts": row["native_case_attempts"],
                          "expected_ordinary_result": row["expected_ordinary_result"], "error": row.get("error"),
                          "controller_fds_before": row["controller_fds_before"], "controller_fds_after": row["controller_fds_after"]}), flush=True)
    if not row["expected_ordinary_result"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
