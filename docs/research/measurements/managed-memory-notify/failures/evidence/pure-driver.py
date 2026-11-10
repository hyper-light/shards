"""Run only argument/arithmetic tests in an admitted NEW private Linux scope."""
import base64
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import time


def main():
    os.umask(0o077)
    private = Path(tempfile.mkdtemp(prefix="managed-notify-failures-pure-", dir=Path.home()))
    os.chdir(private)
    os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
    resource.setrlimit(resource.RLIMIT_AS, (1 << 30, 1 << 30))
    resource.setrlimit(resource.RLIMIT_FSIZE, (2 << 30, 2 << 30))
    payload = json.loads(base64.b64decode(os.environ.pop("PURE_FILES_B64"), validate=True))
    identities = []
    for name in ("launcher.py", "purechecks.py", "nofile-child.c"):
        data = base64.b64decode(payload[name], validate=True)
        Path(name).write_bytes(data)
        identities.append({"name": name, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)})
    record = {
        "schema": "managed-notify-failure-pure-v1",
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "private_directory": str(private), "sources": identities,
        "python": sys.version, "uname": list(os.uname()),
        "affinity": sorted(os.sched_getaffinity(0)),
        "address_space_limit": 1 << 30, "file_size_limit": 2 << 30,
        "commands": [], "native_compile": "UNRUN: ROOT only",
        "native_execution": "UNRUN: ROOT only; imports have no live operations",
    }

    def run(argv, expected):
        result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, check=False, timeout=30)
        record["commands"].append({"argv": argv, "exit_code": result.returncode,
                                   "expected": expected, "output": result.stdout})
        if result.returncode != expected:
            raise RuntimeError("unexpected pure test result")

    try:
        run([sys.executable, "-B", "purechecks.py", "-v"], 0)
        original = Path("launcher.py").read_bytes()
        anchor = b"(nanoseconds + 1_000_000 - 1) // 1_000_000"
        if original.count(anchor) != 1:
            raise RuntimeError("arithmetic mutation anchor changed")
        Path("launcher.py").write_bytes(original.replace(anchor, b"nanoseconds // 1_000_000"))
        run([sys.executable, "-B", "purechecks.py", "PureChecks.test_poll_unit_and_only_abi_clamp"], 1)
        Path("launcher.py").write_bytes(original)
        run([sys.executable, "-B", "purechecks.py", "-v"], 0)
        record["result"] = "PASS: argument/arithmetic only; round-down guard removal failed as expected"
    except Exception as error:
        record["result"] = "FAIL"
        record["error"] = repr(error)
    encoded = json.dumps(record, indent=2) + "\n"
    Path("evidence.json").write_text(encoded)
    print(encoded, end="")
    return 0 if record["result"].startswith("PASS:") else 1


if __name__ == "__main__":
    raise SystemExit(main())
