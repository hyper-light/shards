#!/usr/bin/env python3
"""Two ordinary owned-API interleavings, sharing the existing held-exit driver."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import resource
import sys

SELECTORS = ["owning_zero_max_and_waiters_join_after_stop",
             "stop_with_both_owning_handshakes_pending_joins_every_caller"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--case-seconds", type=int, required=True)
    parser.add_argument("--compile-seconds", type=int, required=True)
    args = parser.parse_args()
    if args.case_seconds <= 0 or args.compile_seconds <= 0:
        parser.error("positive finite bounds required")
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("held_exit", args.helper)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    root = args.root.resolve()
    root.mkdir(mode=0o700)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    bundle = json.loads(args.bundle.read_text())
    if len(bundle["versions"]) != 1:
        raise ValueError("one exact ordinary source required")
    version = bundle["versions"][0]
    if version["has_allocation_refusal"] or any(name.endswith("/native_case.rs") for name in version["files"]):
        raise ValueError("ordinary scope excludes the allocation hook")
    source = root / version["label"]
    source.mkdir()
    for name, record in version["files"].items():
        path = Path(name)
        if path.is_absolute() or ".." in path.parts:
            raise ValueError("unsafe source path")
        body = record["text"].encode()
        if helper.sha(body) != record["sha256"]:
            raise ValueError("source hash mismatch")
        helper.write_new(source / path, body)
    env = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
    env.update(CARGO_HOME=str(root / "cargo-home"), CARGO_TARGET_DIR=str(source / "target"),
               CARGO_BUILD_JOBS="1", CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS="1",
               RUSTFLAGS="-Cllvm-args=-threads=1")
    row = {"revision": version["revision"], "source": str(source), "no_allocator_hook": True,
           "file_identities": {name: {k: v for k, v in rec.items() if k != "text"}
                               for name, rec in version["files"].items()},
           "environment": {k: env[k] for k in ["CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS",
                                              "CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS", "RUSTFLAGS"]}}
    argv = ["cargo", "+1.98.1", "test", "--locked", "-j1", "--profile", "test-release",
            "-p", "shards-api", "--lib", "--no-run", "--message-format=json"]
    row["compile"] = helper.command(argv, source, env, source / "compile", args.compile_seconds)
    if row["compile"]["exit"] == 0:
        artifacts = [json.loads(line) for line in (source / "compile.stdout").read_text().splitlines()
                     if line.startswith("{")]
        binaries = [Path(item["executable"]) for item in artifacts if item.get("reason") == "compiler-artifact"
                    and item.get("executable") and item.get("target", {}).get("name") == "shards_api"]
        if len(binaries) != 1:
            raise RuntimeError("one actual API test artifact required")
        binary = binaries[0]
        row["binary"] = {"path": str(binary), "sha256": helper.sha(binary.read_bytes())}
        row["cases"] = []
        for name in SELECTORS:
            selector = "client::concurrent_sync::" + name
            selected = helper.command([str(binary), selector, "--list", "--exact"], source, env,
                                      source / (name + "-selection"), args.case_seconds)
            listing = (source / (name + "-selection.stdout")).read_text()
            if selected["exit"] != 0 or listing.count(selector + ": test") != 1 or "1 test," not in listing:
                raise RuntimeError("selector did not select exactly one test")
            result = helper.case(binary, source / name, args.case_seconds, False, env, selector=selector)
            output = (source / name / "stdout").read_bytes()
            result["all_threads_joined_marker"] = b"CONCURRENT_JOINED " in output
            result["native_parent_reap_echild"] = b"COLD_NATIVE_REAP_ECHILD\n" in output
            result["vsock_directory_absent"] = bool(result.get("vsock")) and not Path(result["vsock"]).parent.exists()
            row["cases"].append(result)
    helper.write_new(source / "summary.json", (json.dumps(row, indent=2) + "\n").encode())
    passed = row["compile"]["exit"] == 0 and all(c["exit"] == 0 and not c["timed_out"]
        and not c.get("driver_error") and c["test_exit_observed"] and c["standin_exit_observed"]
        and c.get("observer_registered_before_release") and c["all_threads_joined_marker"]
        and c["native_parent_reap_echild"] and c["vsock_directory_absent"] for c in row.get("cases", []))
    print(json.dumps({"source": str(source), "compile": row["compile"]["exit"],
                      "binary": row.get("binary"), "cases": row.get("cases"), "expected_ordinary_results": passed}))
    if not passed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
