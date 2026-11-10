#!/usr/bin/env python3
"""ROOT-only native caller; no build, VM, sweep or production mutation.

One supplied source-hashed test binary, one exact ignored case, one owned result
directory and one original finite deadline. A binary hash identifies bytes; ROOT
must separately retain the build receipt linking those bytes to this source.
"""

import argparse
import errno
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time


CASES = {
    "owner-finished-lock": "native_owner_stop_after_finish_while_metadata_locked",
    "owner-serving-lock": "native_owner_stop_before_finish_while_metadata_locked",
    "completion-lock": "native_completion_wins_then_owner_stops_while_metadata_locked",
    "readonly-file": "native_readonly_file_error_keeps_logical_completion_and_retires_driver",
    "file-limit": "native_file_limit_retains_partial_append_and_known_canonical_errno",
}
SOURCES = (
    "AGENTS.md",
    "docs/development-rules.md",
    "crates/shards/src/daemon/managed.rs",
    "crates/shards/src/daemon/managed/terminal_order_tests.rs",
    "crates/remote/src/managed.rs",
    "Cargo.lock",
)
ENDED = b"member 0 channel 1 state ended accepted 0 consumed 0 tail complete\n"
ABORTED = b"member 0 channel 1 state aborted accepted 0 consumed 0 tail unknown\n"
CANONICAL = f"member 0 channel 1 canonical aborted errno {errno.EIO}\n".encode() + ABORTED


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def positive_seconds(value):
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("positive finite timeout required")
    return parsed


def positive_integer(value):
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("positive integer required")
    return parsed


def expected_bytes(case, prefix):
    if case == "file-limit":
        if prefix is None or not 0 < prefix < len(CANONICAL):
            raise ValueError("file-limit requires a strict nonempty canonical-row prefix")
        return CANONICAL[:prefix]
    if prefix is not None:
        raise ValueError("prefix/child file limit belongs only to file-limit")
    if case == "owner-serving-lock":
        return ABORTED
    if case == "readonly-file":
        return b""
    return ENDED


def test_command(binary, case):
    name = "daemon::managed::terminal_order_tests::" + CASES[case]
    return [str(binary), name, "--exact", "--ignored", "--nocapture", "--test-threads=1"]


def source_identity(root, timeout_seconds):
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=root, check=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, timeout=timeout_seconds,
    ).stdout.decode().strip()
    return {"revision": revision, "sha256": {path: file_hash(root / path) for path in SOURCES}}


def verdict(case, code, output, metadata, expected):
    failures = []
    if code != 0:
        failures.append(f"native exit {code}")
    if f"TERMINAL_ORDER event=main-enter case={case} semantics=empty-unsubscribed".encode() not in output:
        failures.append("actual selected native fixture entry not observed")
    if f"TERMINAL_ORDER result=pass case={case} ".encode() not in output:
        failures.append("selected native completion receipt missing")
    if b"TERMINAL_ORDER event=driver-return result=Ok(())" not in output:
        failures.append("actual successful endpoint return/join receipt missing")
    if not re.search(rb"test result: ok\. 1 passed; 0 failed; 0 ignored;", output):
        failures.append("exactly one passing nonignored execution not observed")
    if metadata != expected:
        failures.append("actual retained metadata bytes differ from independent expectation")
    return failures


def exec_limited(binary, prefix):
    # This branch runs in a fresh disposable Python child, then execs the exact
    # native case. Never alter the caller, runner or another process's limits.
    import resource
    import signal

    expected_bytes("file-limit", prefix)
    _, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
    if hard != resource.RLIM_INFINITY and prefix > hard:
        raise ValueError("caller prefix would raise the inherited child hard limit")
    signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
    resource.setrlimit(resource.RLIMIT_FSIZE, (prefix, prefix))
    print(f"TERMINAL_ORDER event=launcher-limit fsize={prefix} scope=owned-child", flush=True)
    os.execv(binary, test_command(binary, "file-limit"))


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--binary", type=Path, required=True)
    result.add_argument("--binary-sha256", required=True)
    result.add_argument("--source-root", type=Path, required=True)
    result.add_argument("--source-revision", required=True)
    result.add_argument("--output", type=Path, required=True)
    result.add_argument("--case", choices=CASES, required=True)
    result.add_argument("--case-ms", type=positive_integer, required=True)
    result.add_argument("--timeout-seconds", type=positive_seconds, required=True)
    result.add_argument("--prefix-bytes", type=positive_integer)
    return result


def main(argv=None):
    arguments = parser()
    args = arguments.parse_args(argv)
    try:
        expected = expected_bytes(args.case, args.prefix_bytes)
        if args.timeout_seconds <= args.case_ms / 1000:
            raise ValueError("outer timeout must exceed the original native case deadline")
        if args.case == "file-limit" and os.name != "posix":
            raise ValueError("file-limit requires a Unix isolated child; Windows remains unverified")
        binary = args.binary.resolve(strict=True)
        root = args.source_root.resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("supplied test binary must be an executable regular file")
        if not re.fullmatch(r"[0-9a-f]{64}", args.binary_sha256):
            raise ValueError("lowercase SHA256 required")
        if file_hash(binary) != args.binary_sha256:
            raise ValueError("supplied binary hash mismatch")
        source = source_identity(root, args.timeout_seconds)
        if source["revision"] != args.source_revision:
            raise ValueError("supplied source revision mismatch")
        # Existing paths are preserved and rejected, including a previous failure.
        args.output.mkdir(mode=0o700)
        output_dir = args.output.resolve(strict=True)
        owned = output_dir / "owned"
        owned.mkdir(mode=0o700)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        arguments.error(str(error))

    command = test_command(binary, args.case)
    if args.case == "file-limit":
        command = [sys.executable, str(Path(__file__).resolve()), "--_exec-limited", str(binary), str(args.prefix_bytes)]
    environment = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    for key in tuple(environment):
        if key.startswith("SHARDS_TERMINAL_ORDER_"):
            del environment[key]
    environment.update({
        "SHARDS_TERMINAL_ORDER_DIRECTORY": str(owned),
        "SHARDS_TERMINAL_ORDER_CASE_MS": str(args.case_ms),
    })
    if args.prefix_bytes is not None:
        environment["SHARDS_TERMINAL_ORDER_PREFIX_BYTES"] = str(args.prefix_bytes)
    receipt = {
        "case": args.case, "argv": command, "source_before": source,
        "binary": str(binary), "binary_sha256": args.binary_sha256,
        "case_ms": args.case_ms, "outer_timeout_seconds": args.timeout_seconds,
        "prefix_bytes": args.prefix_bytes,
        "limits": "only fresh file-limit child changes RLIMIT_FSIZE/SIGXFSZ; no host settings",
        "semantics": "real empty/unsubscribed Shared, File and endpoint; no VM/data/ACK/FIN claim",
        "build_source_association": "ROOT must retain independent source-matching build receipt",
        "started_unix_ns": time.time_ns(),
    }
    receipt_path = output_dir / "receipt.json"
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
    try:
        result = subprocess.run(
            command, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, timeout=args.timeout_seconds,
        )
        output, code = result.stdout, result.returncode
        receipt["process"] = "exit observed and reaped by subprocess.run"
    except subprocess.TimeoutExpired as error:
        output, code = error.stdout or b"", 124
        receipt["process"] = "deadline expired; subprocess.run killed and waited for owned process"
        receipt["in_process_joins"] = "unknown; inspect partial native receipts"
    except OSError as error:
        output, code = repr(error).encode(), 125
        receipt["process"] = "spawn/launcher failure; native execution unverified"
    (output_dir / "native.log").write_bytes(output)
    metadata_path = owned / "metadata"
    metadata = None
    if metadata_path.exists() and stat.S_ISREG(metadata_path.lstat().st_mode):
        with metadata_path.open("rb") as retained:
            metadata = retained.read(len(CANONICAL) + 1)
        receipt["metadata_file_size"] = metadata_path.stat().st_size
    failures = verdict(args.case, code, output, metadata, expected)
    try:
        receipt["source_after"] = source_identity(root, args.timeout_seconds)
        if receipt["source_after"] != source:
            failures.append("source identity changed during case")
        receipt["binary_sha256_after"] = file_hash(binary)
        if receipt["binary_sha256_after"] != args.binary_sha256:
            failures.append("binary bytes changed during case")
    except (OSError, subprocess.SubprocessError) as error:
        failures.append(f"post-case identity verification failed: {error}")
    receipt.update({
        "exit_code": code, "finished_unix_ns": time.time_ns(),
        "native_log_sha256": sha256(output),
        "metadata_sha256": sha256(metadata) if metadata is not None else None,
        "expected_metadata_hex": expected.hex(), "failures": failures,
        "outcome": "pass" if not failures else "unverified-or-failed",
    })
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"receipt": str(receipt_path), "outcome": receipt["outcome"], "failures": failures}))
    return 0 if not failures else 1


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--_exec-limited":
        exec_limited(sys.argv[2], int(sys.argv[3]))
    else:
        raise SystemExit(main())
