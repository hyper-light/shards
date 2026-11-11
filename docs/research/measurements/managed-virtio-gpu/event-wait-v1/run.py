#!/usr/bin/env python3
"""One admitted native GPU corpus/full VMM lint batch, preserving every stage."""

import hashlib
import json
import os
import platform
import re
import shutil
import signal
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
OUT = HERE / "raw"
TARGET = ROOT / "target/managed-virtio-gpu-event-wait-v1"
OUT.mkdir(exist_ok=False)
TARGET.mkdir(exist_ok=False)
deadline = time.monotonic() + 570
env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
env["CARGO_TARGET_DIR"] = str(TARGET)
env["CARGO_BUILD_JOBS"] = "2"
inputs = json.loads((HERE / "source-inputs.json").read_text())
source_path = ROOT / inputs["owned_test_path"]
source_hash = hashlib.sha256(source_path.read_bytes()).hexdigest()
old_manifest = HERE.parent / "MANIFEST.json"
results = []
receipt = {
    "platform": platform.platform(), "machine": platform.machine(),
    "source_sha256": source_hash, "source_head": inputs["actual_tested_leaf"],
    "comparison_parent": inputs["root_candidate"], "target": str(TARGET),
    "admission": "thisMac 2CPU/2GiB/4GiB/600s/-j2, lockf, free >=30GiB",
    "scope": "GPU-only native unit corpus + strict full VMM lib/tests; no VM/renderer",
    "results": results,
}


def save():
    (HERE / "run.json").write_text(json.dumps(receipt, indent=2) + "\n")


def verify():
    free = shutil.disk_usage(ROOT).free
    if free < 30 * 1024**3:
        raise RuntimeError(f"Mac free-space floor not met: {free}")
    if hashlib.sha256(source_path.read_bytes()).hexdigest() != source_hash:
        raise RuntimeError("owned source changed during the batch")
    if hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest() != inputs["current_lock_sha256"]:
        raise RuntimeError("exact product lock changed")
    if hashlib.sha256(old_manifest.read_bytes()).hexdigest() != inputs["immutable_old_manifest_sha256"]:
        raise RuntimeError("original286 manifest changed")
    return free


def run(name, argv):
    free = verify()
    out = OUT / name
    out.mkdir(exist_ok=False)
    item = {"name": name, "argv": argv, "free_bytes_before": free,
            "controller_fd_before": len(os.listdir("/dev/fd"))}
    results.append(item)
    save()
    with (out / "stdout").open("wb") as stdout, (out / "stderr").open("wb") as stderr:
        child = subprocess.Popen(argv, cwd=ROOT, env=env, stdout=stdout,
                                 stderr=stderr, start_new_session=True)
        item["pid"] = child.pid
        try:
            child.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            item["timeout"] = True
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=15)
        item["exit"] = child.returncode
    try:
        os.waitpid(child.pid, os.WNOHANG)
        item["direct_child_reaped"] = False
    except ChildProcessError:
        item["direct_child_reaped"] = True
    item["controller_fd_after"] = len(os.listdir("/dev/fd"))
    item["free_bytes_after"] = shutil.disk_usage(ROOT).free
    save()
    if item["exit"] != 0 or item.get("timeout"):
        raise RuntimeError(f"{name} failed: {item}")
    verify()
    return out


try:
    verify()
    run("rustc", ["rustc", "-Vv"])
    run("cargo", ["cargo", "-Vv"])
    run("policy", ["scripts/check-development-policy"])
    run("format", ["rustfmt", "--check", "--edition", "2024", str(source_path)])
    built = run("build", ["cargo", "test", "--locked", "-j2", "-p", "shards-vmm",
                          "--lib", "--no-run", "--message-format=json"])
    binaries = []
    for line in (built / "stdout").read_text().splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "shards_vmm" and item.get("executable"):
            binaries.append(Path(item["executable"]))
    if len(binaries) != 1:
        raise RuntimeError(f"expected one actual VMM test executable: {binaries}")
    listing = run("list", ["scripts/hvf-run", str(binaries[0]), "devices::virtio::gpu::", "--list"])
    listed = (listing / "stdout").read_text()
    receipt["listed_gpu_tests"] = len([line for line in listed.splitlines() if line.endswith(": test")])
    frozen = TARGET / "frozen/shards-vmm-gpu-tests"
    frozen.parent.mkdir()
    shutil.copy2(binaries[0], frozen)
    receipt["frozen_binary"] = {"path": str(frozen), "bytes": frozen.stat().st_size,
                                 "sha256": hashlib.sha256(frozen.read_bytes()).hexdigest()}
    save()
    native = run("gpu", [str(frozen), "devices::virtio::gpu::", "--nocapture", "--test-threads=1"])
    stdout = (native / "stdout").read_text()
    match = re.search(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout)
    receipt["gpu_counts"] = [int(match[index]) for index in range(1, 4)] if match else None
    receipt["used_head_witness"] = [line for line in stdout.splitlines() if "before_new_signal=" in line]
    save()
    if receipt["gpu_counts"] != [receipt["listed_gpu_tests"], 0, 0] or not receipt["used_head_witness"]:
        raise RuntimeError("native corpus result/positive used-head witness missing")
    run("clippy", ["cargo", "clippy", "--locked", "-j2", "-p", "shards-vmm", "--lib", "--tests", "--", "-D", "warnings"])
    verify()
    receipt["outcome"] = "complete"
except Exception as error:
    receipt["outcome"] = "failed"
    receipt["error"] = str(error)
    save()
    raise
save()
print(json.dumps({"outcome": receipt["outcome"], "source_sha256": source_hash,
                  "gpu_counts": receipt.get("gpu_counts"),
                  "binary": receipt.get("frozen_binary"), "stages": results}, sort_keys=True))
