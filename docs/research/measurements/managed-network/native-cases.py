#!/usr/bin/env python3
"""Coordinator-only native endpoint cases; this does not prove VM mediation."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--within-ms", type=int, required=True)
    parser.add_argument("--revision", required=True)
    args = parser.parse_args()
    if args.within_ms <= 0:
        parser.error("the coordinator must supply a positive case budget")
    args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    binary = args.binary.resolve()
    rows = []

    def case(name, words, expected, no_listen=False):
        command = [str(binary), "managed-network", *words]
        start = time.monotonic_ns()
        out = args.out / (name + ".stdout")
        err = args.out / (name + ".stderr")
        with out.open("wb") as stdout, err.open("wb") as stderr:
            try:
                result = subprocess.run(
                    command, stdout=stdout, stderr=stderr,
                    timeout=args.within_ms / 1000,
                )
                status = result.returncode
            except subprocess.TimeoutExpired:
                status = "parent-budget-expired"
        stdout = out.read_bytes()
        stderr = err.read_bytes()
        passed = (
            isinstance(status, int)
            and ((status == 0) == expected)
            and (not no_listen or b"Listening" not in stdout)
        )
        rows.append({
            "case": name, "command": command, "status": status,
            "passed": passed, "elapsed_ns": time.monotonic_ns() - start,
            "stdout": out.name, "stderr": err.name,
            "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
            "stderr_sha256": hashlib.sha256(stderr).hexdigest(),
        })

    budget = str(args.within_ms)
    byte_values = bytes(range(1 << 8))
    for family, address, most in [
        ("v4", "127.0.0.1:0", (1 << 16) - 1 - 20 - 8),
        ("v6", "[::1]:0", (1 << 16) - 1 - 8),
    ]:
        # IPv4 total-length minus its minimum header and UDP; IPv6 payload minus UDP.
        maximum = (byte_values * ((most + len(byte_values) - 1) // len(byte_values)))[:most]
        for transport in ["tcp", "udp", "udp-connected"]:
            for label, payload in [
                ("empty", b""),
                ("binary", b"request\0\n\xff\x80\x1b[2Jtail"),
                ("largest-udp", maximum),
            ]:
                case(f"{family}-{transport}-{label}", [
                    "pair", transport, address, payload.hex(),
                    payload[::-1].hex(), budget, "native",
                ], True)
            case(f"{family}-{transport}-zero-deadline", [
                "pair", transport, address, "", "", "0", "native",
            ], False, no_listen=True)
        case(f"{family}-udp-too-large", [
            "pair", "udp", address, (maximum + b"x").hex(), "", budget, "native",
        ], False)
    case("malformed-hex", [
        "pair", "tcp", "127.0.0.1:0", "ff0", "", budget, "native",
    ], False, no_listen=True)
    manifest = {
        "scope": "native endpoint behavior only; no VM/interception/performance claim",
        "revision": args.revision, "platform": platform.platform(),
        "euid": os.geteuid(), "within_ms": args.within_ms,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "results": rows,
    }
    (args.out / "results.json").write_text(json.dumps(manifest, indent=2) + "\n")
    failed = [row["case"] for row in rows if not row["passed"]]
    print(json.dumps({"cases": len(rows), "failed": failed, "evidence": str(args.out)}))
    return bool(failed)


if __name__ == "__main__":
    raise SystemExit(main())
