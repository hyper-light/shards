#!/usr/bin/env python3
"""Admitted remote PURE tests only. Never compiles or invokes native::main.

Input SOURCE_B64 is fixture.rs; all files go into a newly created private folder.
The caller admits <=1 CPU/1 GiB/2 GiB through Kiln, with sync=False.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import tempfile
import time


def digest(data):
    return hashlib.sha256(data).hexdigest()


def main():
    os.umask(0o077)
    private = Path(tempfile.mkdtemp(prefix="managed-memory-notify-pure-", dir=Path.home()))
    os.chdir(private)
    affinity = os.sched_getaffinity(0)
    os.sched_setaffinity(0, {min(affinity)})
    resource.setrlimit(resource.RLIMIT_AS, (1 << 30, 1 << 30))
    resource.setrlimit(resource.RLIMIT_FSIZE, (2 << 30, 2 << 30))
    source = base64.b64decode(os.environ.pop("SOURCE_B64"), validate=True)
    Path("fixture.rs").write_bytes(source)
    record = {
        "schema": "managed-memory-notify-pure-v1",
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "private_directory": str(private),
        "source_sha256": digest(source),
        "uname": list(os.uname()),
        "cpu_affinity": sorted(os.sched_getaffinity(0)),
        "address_space_limit": 1 << 30,
        "file_size_limit": 2 << 30,
        "commands": [],
        "native_compile": "UNRUN: cfg(not(test)) module excluded",
        "native_execution": "UNRUN: ROOT only",
    }

    def command(argv, expected=0):
        result = subprocess.run(argv, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=60, check=False)
        record["commands"].append({"argv": argv, "exit_code": result.returncode,
                                   "expected": expected, "output": result.stdout})
        if result.returncode != expected:
            raise RuntimeError(f"unexpected exit {result.returncode}: {argv}")
        return result.stdout.strip()

    try:
        rustc = command(["rustup", "which", "rustc"])
        rustfmt = command(["rustup", "which", "rustfmt"])
        command([rustc, "--version", "--verbose"])
        command([rustfmt, "--version"])
        # Canonical main69dbc rustfmt.toml: max_width = 110. No repository sync.
        command([rustfmt, "--edition", "2024", "--config", "max_width=110",
                 "--check", "fixture.rs"])
        command([rustc, "--edition=2024", "--test", "fixture.rs", "-C", "codegen-units=1",
                 "-o", "pure-tests"])
        command(["./pure-tests", "--test-threads=1"])
        # Both mutations are confined to synthetic tests in new source copies.
        guard = b"if !name.is_empty() && !name.contains(&0) {"
        if source.count(guard) != 1:
            raise RuntimeError("decoder mutation anchor changed")
        mutant = source.replace(guard, b"if false {")
        Path("missing-nul-mutant.rs").write_bytes(mutant)
        command([rustc, "--edition=2024", "--test", "missing-nul-mutant.rs", "-C",
                 "codegen-units=1", "-o", "missing-nul-mutant"])
        command(["./missing-nul-mutant", "--test-threads=1",
                 "tests::unterminated_name_and_huge_claim_refused", "--exact"], expected=101)
        guard = b"u128::from(left.subsec_nanos() % 1_000_000 != 0)"
        if source.count(guard) != 1:
            raise RuntimeError("deadline mutation anchor changed")
        mutant = source.replace(guard, b"0")
        Path("round-down-mutant.rs").write_bytes(mutant)
        command([rustc, "--edition=2024", "--test", "round-down-mutant.rs", "-C",
                 "codegen-units=1", "-o", "round-down-mutant"])
        command(["./round-down-mutant", "--test-threads=1",
                 "tests::poll_rounds_up_and_clamps_only_abi_bound", "--exact"], expected=101)
        command(["./pure-tests", "--test-threads=1"])
        record["result"] = "PASS: pure tests and targeted guard removals only"
    except Exception as error:
        record["result"] = "FAIL"
        record["error"] = repr(error)
    record["artifact_bytes"] = sum(p.stat().st_size for p in private.iterdir() if p.is_file())
    if record["artifact_bytes"] > 2 << 30:
        record["result"] = "FAIL: admitted artifact envelope exceeded"
    encoded = json.dumps(record, indent=2) + "\n"
    Path("evidence.json").write_text(encoded)
    print(encoded, end="")
    return 0 if record["result"].startswith("PASS:") else 1


if __name__ == "__main__":
    raise SystemExit(main())
