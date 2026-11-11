#!/usr/bin/env python3
"""Retain one admitted, finite GPU-only native check. Invoke under the Mac lock."""
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
HERE = Path(__file__).resolve().parent
label, *command = sys.argv[1:]
if not label or not command or Path(label).name != label:
    raise SystemExit("expected a fresh label and explicit command")
output = HERE / "raw" / label
output.mkdir(parents=True, exist_ok=False)
free = shutil.disk_usage(ROOT).free
if platform.system() == "Darwin" and free < 30 * 1024**3:
    raise SystemExit("Mac free-space floor: 30 GiB")

def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)

def digest(data):
    return hashlib.sha256(data).hexdigest()

lock = (ROOT / "Cargo.lock").read_bytes()
if lock != git("show", "HEAD:Cargo.lock"):
    raise SystemExit("Cargo.lock differs from the exact product lock")
paths = ["Cargo.lock", "docs/development-rules.md",
         "crates/vmm/src/devices/virtio/gpu/protocol.rs",
         "crates/vmm/src/devices/virtio/gpu/mod.rs"]
receipt = {"head": git("rev-parse", "HEAD").decode().strip(),
           "command": command, "platform": platform.platform(),
           "machine": platform.machine(), "free_bytes_before": free,
           "scope": "native GPU-only unit/light checks; no VM or benchmark",
           "admission": "kiln 2CPU/2GiB/4GiB, 600s outer; cargo -j2",
           "hashes": {p: digest((ROOT / p).read_bytes()) for p in paths}}
(output / "source.diff").write_bytes(git("diff", "HEAD", "--", "crates/vmm/src/devices/virtio/gpu"))
(output / "protocol.rs.txt").write_bytes((ROOT / paths[2]).read_bytes())
(output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
env = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
env["CARGO_TARGET_DIR"] = str(ROOT / "target" / "managed-virtio-gpu-light")
env["CARGO_BUILD_JOBS"] = "2"
receipt["target_dir"] = env["CARGO_TARGET_DIR"]
with (output / "stdout").open("wb") as out, (output / "stderr").open("wb") as err:
    try:
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=out, stderr=err,
                                timeout=570, check=False)
        receipt["exit_code"] = result.returncode
    except subprocess.TimeoutExpired:
        receipt["outcome"] = "timeout; no pass inferred"
        receipt["exit_code"] = 124
receipt["free_bytes_after"] = shutil.disk_usage(ROOT).free
(output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps({"output": str(output), "exit_code": receipt["exit_code"]}))
raise SystemExit(receipt["exit_code"])
