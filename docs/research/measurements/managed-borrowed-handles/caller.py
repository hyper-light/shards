#!/usr/bin/env python3
"""ROOT-only Windows caller. Raw point counts do not establish leak freedom.

One source-hashed binary, one exact ignored mode, explicit count and original
case/call/cleanup/outer budgets. No build, VM, warm-up, threshold or host change.
ROOT retains a separate build receipt associating this binary with source bytes.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import time


MODES = {"success": "native_success_handles", "wrong-start": "native_wrong_start_handles",
         "withheld": "native_withheld_reply_handles"}
MARKER = b"BORROWED_HANDLES "
# Winsock's WSAEWOULDBLOCK ABI constant, not a learned threshold or runtime policy.
WSAEWOULDBLOCK = 10035
SOURCES = ("AGENTS.md", "docs/development-rules.md", "Cargo.lock", "crates/api/Cargo.toml",
           "crates/api/src/client.rs", "crates/api/src/lib.rs", "crates/api/src/stream.rs",
           "crates/afunix/src/lib.rs", "docs/research/measurements/managed-borrowed-handles/focused.rs")


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_identity(root, timeout):
    revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, check=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout).stdout.decode().strip()
    return {"revision": revision, "sha256": {path: file_hash(root / path) for path in SOURCES}}


def positive(value):
    parsed = int(value)
    if not 0 < parsed <= (1 << 64) - 1:
        raise argparse.ArgumentTypeError("positive u64 input required")
    return parsed


def seconds(value):
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("positive finite timeout required")
    return parsed


def budget_error(case_ms, call_ms, cleanup_ms, outer):
    if not 0 < cleanup_ms < case_ms <= (1 << 31) - 1:
        return "case/cleanup exceeds finite positive WSAPoll millisecond envelope"
    if not 0 < call_ms <= case_ms - cleanup_ms:
        return "positive call budget must fit the original case minus cleanup reservation"
    if not math.isfinite(outer) or outer <= case_ms / 1000:
        return "native outer timeout must exceed original whole-case budget"
    return None


def duplicate_free(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate native JSON key {key}")
        result[key] = value
    return result


def parse_native(output):
    records, errors = [], []
    for line in output.splitlines():
        at = line.find(MARKER)
        if at < 0:
            continue
        try:
            value = json.loads(line[at + len(MARKER):], object_pairs_hook=duplicate_free)
            if type(value) is not dict:
                raise ValueError("native record must be an object")
            records.append(value)
        except (UnicodeError, ValueError) as error:
            errors.append(f"malformed actual native record: {error}")
    return records, errors


def integer(value, bits, signed=False):
    lower = -(1 << (bits - 1)) if signed else 0
    upper = (1 << (bits - (1 if signed else 0))) - 1
    return type(value) is int and lower <= value <= upper


def validate(mode, repetitions, code, output):
    records, issues = parse_native(output)
    if code != 0:
        issues.append(f"native process exit {code}")
    if not re.search(rb"test result: ok\. 1 passed; 0 failed; 0 ignored;", output):
        issues.append("exactly one passing selected native test was not observed")
    identities = [r for r in records if r.get("kind") == "identity"]
    if len(identities) != 1 or identities[0].get("mode") != mode:
        issues.append("actual selected native identity/mode entry missing")
    elif not integer(identities[0].get("pid"), 32) or identities[0]["pid"] == 0 or not integer(identities[0].get("started"), 64):
        issues.append("native current-process PID/start representation invalid or unknown")
    counts = [r for r in records if r.get("kind") == "count"]
    expected = [("cold", None), ("setup", None)]
    expected += [("after-operation", n) for n in range(repetitions)]
    expected += [("after-operations", None), ("post-join", None)]
    if [(r.get("phase"), r.get("iteration")) for r in counts] != expected:
        issues.append("raw cold/setup/operation/post-join sequence incomplete, duplicated or reordered")
    for sample in counts:
        if not integer(sample.get("returned"), 32, True):
            issues.append("native count BOOL representation invalid")
            continue
        if sample["returned"] == 0:
            if sample.get("count") is not None or not integer(sample.get("error"), 32):
                issues.append("failed GetProcessHandleCount must retain native error and unknown count")
            issues.append("actual GetProcessHandleCount failed; count is unknown")
        elif not integer(sample.get("count"), 32) or sample.get("error") is not None:
            issues.append("successful GetProcessHandleCount requires raw DWORD and no fabricated error")
        iteration = sample.get("iteration")
        if iteration is not None and not integer(iteration, 64):
            issues.append("native iteration representation invalid")
    operations = [r for r in records if r.get("kind") == "operation"]
    if [r.get("iteration") for r in operations] != list(range(repetitions)):
        issues.append("complete ordered operation receipts missing")
    api = {"success": "connected", "wrong-start": "different-start", "withheld": "read-expired"}[mode]
    for operation in operations:
        if not integer(operation.get("iteration"), 64):
            issues.append("native operation iteration representation invalid")
        for key in ("close_returned", "close_error", "no_dial_error"):
            value = operation.get(key)
            if value is not None and not integer(value, 32, True):
                issues.append("native close/accept result representation invalid")
        if operation.get("api") != api:
            issues.append("actual typed connector result differs from selected positive mode")
        if mode == "wrong-start":
            if (operation.get("server_receipt"), operation.get("server_eof_and_close_confirmed"),
                operation.get("close_returned"), operation.get("close_error"), operation.get("no_dial_error")) != (
                    "no-dial", False, None, None, WSAEWOULDBLOCK):
                issues.append("actual one-shot native before-dial refusal receipt missing")
        elif (operation.get("server_receipt"), operation.get("server_eof_and_close_confirmed"),
              operation.get("close_returned"), operation.get("close_error"), operation.get("no_dial_error")) != (
                  "closed", True, 0, None, None):
            issues.append("actual server EOF/close0 acknowledgement missing; resource outcome unknown")
        for boolean in ("server_eof_and_close_confirmed",):
            if type(operation.get(boolean)) is not bool:
                issues.append("native cleanup boolean representation invalid")
        detail = operation.get("raw_debug_hex", "")
        if type(detail) is not str or not re.fullmatch(r"[0-9a-f]+", detail) or len(detail) % 2:
            issues.append("actual typed/native raw receipt absent")
    results = [r for r in records if r.get("kind") == "result"]
    if len(results) != 1:
        issues.append("one final native result required")
    else:
        result = results[0]
        if result.get("mode") != mode:
            issues.append("final native mode mismatch")
        if any(result.get(key) is not True for key in ("functional_pass", "server_joined", "counts_known")):
            issues.append("native operation/observation or actual server join failed or remains unknown")
        if result.get("listener_close_rc", "missing") is not None:
            issues.append("listener Drop cannot be represented as an observed native close return")
        if result.get("handle_stability") != "raw-point-counts-only":
            issues.append("unsupported handle-stability inference in native result")
    if any(r.get("kind") == "failure" for r in records):
        issues.append("actual native fixture failure retained")
    known = {"identity", "count", "listener-flags", "listener-inheritance", "operation", "operation-detail", "failure", "result"}
    if any(r.get("kind") not in known for r in records):
        issues.append("unknown native record kind")
    return records, issues


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--binary-sha256", required=True)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=MODES, required=True)
    parser.add_argument("--repetitions", type=positive, required=True)
    parser.add_argument("--case-ms", type=positive, required=True)
    parser.add_argument("--call-ms", type=positive, required=True)
    parser.add_argument("--cleanup-ms", type=positive, required=True)
    parser.add_argument("--timeout-seconds", type=seconds, required=True)
    args = parser.parse_args()
    try:
        if os.name != "nt":
            raise ValueError("actual native fixture execution belongs to ROOT on Windows only")
        error = budget_error(args.case_ms, args.call_ms, args.cleanup_ms, args.timeout_seconds)
        if error:
            raise ValueError(error)
        binary, root = args.binary.resolve(strict=True), args.source_root.resolve(strict=True)
        if not binary.is_file() or not re.fullmatch(r"[0-9a-f]{64}", args.binary_sha256) or file_hash(binary) != args.binary_sha256:
            raise ValueError("actual binary/hash mismatch")
        source = source_identity(root, args.timeout_seconds)
        if source["revision"] != args.source_revision:
            raise ValueError("actual source revision mismatch")
        socket = args.output.absolute() / "owned" / "v"
        # Existing AF_UNIX SOCKADDR_UN sun_path has 108 bytes, including NUL.
        if len(str(socket).encode("utf8")) >= 108:
            raise ValueError("choose a shorter new private output path for actual AF_UNIX sun_path")
        args.output.mkdir(mode=0o700)
        output = args.output.resolve(strict=True)
        (output / "owned").mkdir(mode=0o700)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.error(str(error))
    command = [str(binary), "client::borrowed_handle_tests::" + MODES[args.mode],
               "--exact", "--ignored", "--nocapture", "--test-threads=1"]
    environment = {k: v for k, v in os.environ.items()
                   if not k.startswith(("DYLD_", "SHARDS_BORROWED_HANDLES_"))}
    environment.update({"SHARDS_BORROWED_HANDLES_REPETITIONS": str(args.repetitions),
                        "SHARDS_BORROWED_HANDLES_CASE_MS": str(args.case_ms),
                        "SHARDS_BORROWED_HANDLES_CALL_MS": str(args.call_ms),
                        "SHARDS_BORROWED_HANDLES_CLEANUP_MS": str(args.cleanup_ms),
                        "SHARDS_BORROWED_HANDLES_SOCKET": str(socket)})
    receipt = {"argv": command, "source_before": source, "binary_sha256": args.binary_sha256,
               "mode": args.mode, "repetitions": args.repetitions,
               "budgets": {"case_ms": args.case_ms, "call_ms": args.call_ms,
                           "cleanup_ms": args.cleanup_ms, "native_outer_seconds": args.timeout_seconds},
               "build_source_association": "requires ROOT's independent source-matching build receipt",
               "started_unix_ns": time.time_ns()}
    path = output / "receipt.json"
    path.write_text(json.dumps(receipt, indent=2) + "\n")
    try:
        result = subprocess.run(command, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=args.timeout_seconds)
        data, code = result.stdout, result.returncode
        receipt["process"] = "actual owned process exited and was waited for"
    except subprocess.TimeoutExpired as error:
        data, code = error.stdout or b"", 124
        receipt["process"] = "native deadline expired; subprocess.run killed and waited for owned process"
        receipt["cleanup"] = "in-process close/join may remain unknown; no kernel-return claim"
    except OSError as error:
        data, code = repr(error).encode(), 125
        receipt["process"] = "launch failed; selected native execution unverified"
    (output / "native.log").write_bytes(data)
    records, issues = validate(args.mode, args.repetitions, code, data)
    (output / "observations.json").write_text(json.dumps(records, indent=2) + "\n")
    try:
        receipt["source_after"] = source_identity(root, args.timeout_seconds)
        receipt["binary_sha256_after"] = file_hash(binary)
        if receipt["source_after"] != source or receipt["binary_sha256_after"] != args.binary_sha256:
            issues.append("source or binary bytes changed during case")
    except (OSError, subprocess.SubprocessError) as error:
        issues.append(f"post-case identity unknown: {error}")
    receipt.update({"exit_code": code, "finished_unix_ns": time.time_ns(),
                    "native_log_sha256": hashlib.sha256(data).hexdigest(), "issues": issues,
                    "functional_outcome": "pass" if not issues else "failed-or-unverified",
                    "handle_stability_verdict": "not established by raw point counts"})
    path.write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"receipt": str(path), "functional_outcome": receipt["functional_outcome"],
                      "handle_stability_verdict": receipt["handle_stability_verdict"], "issues": issues}))
    return 0 if not issues else 1


if __name__ == "__main__":
    raise SystemExit(main())
