#!/usr/bin/env python3
"""Finite native source/component checks; never starts a CLI or VM."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
OUT = HERE / "raw" / sys.argv[1]
OUT.mkdir(exist_ok=False)
start = time.monotonic()
commands = []
free = shutil.disk_usage(ROOT).free
receipt = {
    "host": platform.platform(),
    "machine": platform.machine(),
    "scope": "2CPU/2GiB/4GiB <=600s, one compiler, no native CLI/VM",
    "mac_lock": "/tmp/g121-mac.lock held by invoking lockf",
    "free_bytes": free,
    "deadline_seconds": 600,
}
(OUT / "admission.json").write_text(json.dumps(receipt, indent=2) + "\n")
if platform.system() == "Darwin" and free < 30 * 1024**3:
    raise SystemExit("Mac storage floor: no check started")


def run(name, argv):
    remaining = 600 - (time.monotonic() - start)
    with (OUT / (name + ".stdout")).open("wb") as stdout, (OUT / (name + ".stderr")).open("wb") as stderr:
        result = subprocess.run(argv, cwd=ROOT, stdout=stdout, stderr=stderr, timeout=max(remaining, 0.001))
    commands.append({"argv": [str(a) for a in argv], "exit": result.returncode})
    (OUT / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
    if result.returncode:
        raise SystemExit(result.returncode)


sources = sorted((ROOT / "crates/server/src/harness").glob("*.rs"))
run("format", ["rustfmt", "--edition=2024", *sources])
manifest = {}
for source in sources + [HERE / "fixture.rs", ROOT / "crates/init/src/json.rs", ROOT / "Cargo.lock", Path(__file__).resolve()]:
    payload = source.read_bytes()
    manifest[str(source.relative_to(ROOT))] = {"bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}
(OUT / "sources.json").write_text(json.dumps(manifest, indent=2) + "\n")
run("build", ["rustc", "--edition=2024", "--test", HERE / "fixture.rs", "-D", "warnings", "-o", OUT / "codec-tests"])
run("tests", [OUT / "codec-tests", "harness::tests::", "--nocapture", "--test-threads=1"])
clippy = subprocess.check_output(["rustup", "which", "clippy-driver"], text=True).strip()
denied = ["warnings", *["clippy::" + lint for lint in ["unwrap_used", "expect_used", "panic", "panic_in_result_fn", "todo", "unimplemented", "unreachable", "indexing_slicing", "string_slice", "print_stdout", "print_stderr"]]]
flags = [part for lint in denied for part in ["-D", lint]]
for name, kind in [("production", ["--crate-type=lib"]), ("test-clippy", ["--test"])]:
    run(name, [clippy, "--edition=2024", *kind, HERE / "fixture.rs", "--emit=metadata", "-o", OUT / (name + ".rmeta"), *flags])
run("policy", [ROOT / "scripts/check-development-policy"])
receipt["elapsed_seconds"] = time.monotonic() - start
receipt["commands"] = len(commands)
receipt["exit"] = 0
(OUT / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps({"output": str(OUT), "exit": 0, "commands": len(commands)}))
