#!/usr/bin/env python3
"""ROOT-only baseline runner; this builds and executes native descriptor work."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def distribution(values):
    ordered = sorted(values)
    if not ordered:
        return {"n": 0, "p50": None, "p90": None, "p99": None, "max": None}
    # Nearest rank: ceil(percent * n / 100), one-based. Raw samples stay retained.
    def rank(percent):
        return ordered[(percent * len(ordered) + 99) // 100 - 1]
    return {"n": len(ordered), "p50": rank(50), "p90": rank(90),
            "p99": rank(99), "max": ordered[-1]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("work", type=Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--ready-counts", required=True)
    parser.add_argument("--rounds", required=True)
    parser.add_argument("--deadline-ms", required=True)
    parser.add_argument("--max-records", required=True)
    parser.add_argument("--command-timeout-seconds", required=True, type=float)
    args = parser.parse_args()
    repo = args.repo.resolve()
    work = args.work.resolve()
    if not math.isfinite(args.command_timeout_seconds) or args.command_timeout_seconds <= 0:
        parser.error("command timeout must be finite and positive")
    supported = {"x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl",
                 "aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl",
                 "aarch64-apple-darwin"}
    if args.target not in supported:
        parser.error("target is outside this Unix baseline")
    source = Path(__file__).resolve().parent
    if source != repo / "docs/research/measurements/managed-poller-cost":
        parser.error("run the harness from the named source checkout")
    actual_revision = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
    if actual_revision != args.revision:
        parser.error(f"revision mismatch: {actual_revision}")
    for extra in [[], ["--cached"]]:
        clean = subprocess.run(["git", "-C", str(repo), "diff", *extra, "--quiet"], check=False)
        if clean.returncode != 0:
            parser.error("tracked source must be clean before building")
    work.mkdir(parents=True, exist_ok=False)
    fixture = work / "fixture"
    fixture.mkdir()
    for name in ["Cargo.toml", "fixture.rs", "accounting.rs"]:
        shutil.copyfile(source / name, fixture / name)
    manifest = (fixture / "Cargo.toml").read_text()
    relative = 'path = "../../../../crates/vmm"'
    if manifest.count(relative) != 1:
        raise ValueError("unexpected harness dependency declaration")
    (fixture / "Cargo.toml").write_text(manifest.replace(
        relative, "path = " + json.dumps(str(repo / "crates/vmm"))))
    named = ["Cargo.toml", "Cargo.lock", "crates/vmm/src/platform/mod.rs",
             "crates/vmm/src/platform/unix.rs", "crates/vmm/Cargo.toml"]
    source_hashes = {name: digest(repo / name) for name in named}
    batches = re.findall(r"const AT_ONCE: usize = ([0-9]+);",
                         (repo / "crates/vmm/src/platform/unix.rs").read_text())
    if len(batches) != 1:
        raise ValueError("cannot identify the baseline native batch source token")
    metadata = {"revision": actual_revision, "source_hashes": source_hashes,
                "harness_hashes": {name: digest(source / name) for name in
                                   ["Cargo.toml", "fixture.rs", "accounting.rs", "run.py"]},
                "system": platform.platform(), "architecture": platform.machine(),
                "target": args.target, "work": str(work),
                "source_native_batch_token": batches[0],
                "inputs": {"ready_counts": args.ready_counts, "rounds": args.rounds,
                           "deadline_ms": args.deadline_ms, "max_records": args.max_records},
                "scope": "baseline socket-pair readiness/drain only; no process-exit, fairness, allocation, CPU, fault, kernel-charge or competing-baseline claim",
                "commands": []}
    environment = dict(os.environ, CARGO_BUILD_JOBS="1")
    environment = {key: value for key, value in environment.items() if not key.startswith("DYLD_")}

    def command(argv, label):
        try:
            result = subprocess.run(argv, cwd=work, env=environment, capture_output=True,
                                    timeout=args.command_timeout_seconds, check=False)
            stdout, stderr, status = result.stdout, result.stderr, result.returncode
        except subprocess.TimeoutExpired as error:
            stdout, stderr, status = error.stdout or b"", error.stderr or b"", "Timeout"
        for stream, data in [("stdout", stdout), ("stderr", stderr)]:
            (work / f"{label}.{stream}").write_bytes(data)
        metadata["commands"].append({"argv": argv, "exit": status,
                                     "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
                                     "stderr_sha256": hashlib.sha256(stderr).hexdigest()})
        (work / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
        return status

    for argv, label in [(["rustc", "-vV"], "rustc"), (["cargo", "-V"], "cargo"),
                        (["git", "-C", str(repo), "status", "--porcelain=v1"], "source-status")]:
        if command(argv, label) != 0:
            return 1
    build = ["cargo", "build", "--offline", "--release", "--manifest-path",
             str(fixture / "Cargo.toml"), "--target-dir", str(work / "target"),
             "--target", args.target, "--jobs", "1"]
    if command(build, "build") != 0:
        return 1
    metadata["resolved_lock_sha256"] = digest(fixture / "Cargo.lock")
    binary = work / "target" / args.target / "release/managed-poller-cost"
    metadata["binary_sha256"] = digest(binary)
    invocation = [str(binary), args.ready_counts, args.rounds, args.deadline_ms, args.max_records]
    status = command(invocation, "baseline")
    # A timeout may interrupt cleanup: metadata records it and never calls it returned.
    cases = []
    parse_error = None
    try:
        cases = [json.loads(line) for line in (work / "baseline.stdout").read_text().splitlines()]
    except (ValueError, UnicodeError) as error:
        parse_error = str(error)
    complete = status == 0 and parse_error is None and len(cases) == len(args.ready_counts.split(',')) and all(c["complete"] for c in cases)
    waits = [sample for case in cases for sample in case["waits"]] if complete else []
    observed = [sample for wait in waits for sample in wait["observations"]]
    summary = {"case_records": len(cases), "complete": complete,
               "parse_error": parse_error, "wait_ns": distribution([w["wait_ns"] for w in waits]),
               "write_completed_to_wait_return_ns": distribution(
                   [o["write_completed_to_wait_return_ns"] for o in observed]),
               "wait_return_to_drained_ns": distribution(
                   [o["wait_return_to_drained_ns"] for o in observed]),
               "percentiles": "nearest rank; suppressed for an incomplete invocation; raw JSONL retained; recording/dispatch work is not subtracted"}
    (work / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if summary["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
