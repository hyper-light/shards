#!/usr/bin/env python3
"""Build/list the one native fixture in a new private admitted scope; no case run."""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time

root = Path(sys.argv[1])
deadline = float(sys.argv[2])
source = root / "source"
records = root / "build-records"
records.mkdir()


def save(name, value):
    with (records / name).open("x") as file:
        json.dump(value, file, indent=2)
        file.write("\n")


def stage(name, argv, seconds, env):
    remaining = deadline - time.time()
    if remaining <= 0:
        raise RuntimeError("original600s scope expired before " + name)
    timeout = min(seconds, remaining)
    save(name + ".command.json", {"argv": argv, "cwd": str(source), "timeout_s": timeout})
    start = time.time()
    with (records / (name + ".stdout")).open("xb") as out, (records / (name + ".stderr")).open("xb") as err:
        try:
            result = subprocess.run(argv, cwd=source, env=env, stdout=out, stderr=err, timeout=timeout)
            code = result.returncode
            expired = False
        except subprocess.TimeoutExpired:
            code = None
            expired = True
    save(name + ".result.json", {"exit_code": code, "timeout": expired, "elapsed_s": time.time() - start})
    if expired or code != 0:
        raise RuntimeError(name + " did not complete0; original outputs retained")


try:
    assert platform.system() == "Darwin" and platform.machine() == "arm64"
    floor = 30 * 1024**3
    usage = os.statvfs(root)
    free = usage.f_bavail * usage.f_frsize
    assert free >= floor
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("DYLD_"):
            del env[key]
    env.update(CARGO_HOME=str(root / "cargo-home"), CARGO_TARGET_DIR=str(root / "target"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0")
    save("environment.json", {"platform": platform.platform(), "machine": platform.machine(), "free_bytes": free,
                              "source": str(source), "target": env["CARGO_TARGET_DIR"], "cargo_home": env["CARGO_HOME"],
                              "original_deadline_epoch": deadline, "native_case_run": False})
    stage("toolchain", ["rustc", "--version", "--verbose"], 30, env)
    stage("policy", ["python3", "scripts/check-development-policy"], 30, env)
    cargo = ["cargo", "test", "-p", "shards-vmm", "--lib", "--profile", "test-release", "--target", "aarch64-apple-darwin", "-j2", "--no-run", "--message-format=json"]
    stage("build", cargo, 180, env)
    artifacts = []
    for line in (records / "build.stdout").read_text().splitlines():
        if line.startswith("{"):
            event = json.loads(line)
            if event.get("reason") == "compiler-artifact" and event.get("executable") and event.get("target", {}).get("name") == "shards_vmm":
                artifacts.append(event)
    assert len(artifacts) == 1
    executable = Path(artifacts[0]["executable"])
    raw = executable.read_bytes()
    (root / "artifacts").mkdir()
    copied = root / "artifacts" / "shards-vmm-native"
    with copied.open("xb") as file:
        file.write(raw)
    copied.chmod(0o755)
    sha = hashlib.sha256(raw).hexdigest()
    save("artifact.json", {"cargo_artifact": artifacts[0], "frozen_executable": str(copied), "bytes": len(raw), "sha256": sha})
    name = "managed_audit_wire_adversarial::full_fuse_frames_share_one_retained_audit_budget"
    stage("list", [str(copied), name, "--exact", "--ignored", "--list"], 30, env)
    listed = (records / "list.stdout").read_text()
    assert [line for line in listed.splitlines() if line.endswith(": test")] == [name + ": test"]
    clippy = ["cargo", "clippy", "-p", "shards-vmm", "--lib", "--tests", "--profile", "test-release", "--target", "aarch64-apple-darwin", "-j2", "--", "-Dwarnings"]
    stage("clippy", clippy, 180, env)
    final_lock = (source / "Cargo.lock").read_bytes()
    with (records / "actual-Cargo.lock").open("xb") as file:
        file.write(final_lock)
    usage = os.statvfs(root)
    assert usage.f_bavail * usage.f_frsize >= floor
    ready = {"status": "BuiltListedExactlyOne_NoCaseRun", "executable": str(copied), "sha256": sha,
             "filter": name, "selected_count": 1, "original_deadline_epoch": deadline,
             "argv": [str(copied), name, "--exact", "--ignored", "--nocapture", "--test-threads=1"],
             "case_environment": {"SHARDS_AUDIT_WIRE_CORPUS": str(root / "input" / "data"),
                                  "SHARDS_AUDIT_WIRE_RECORDS": str(root / "native-records-v1"),
                                  "TMPDIR": str(root / "native-tmp-v1")},
             "case_parent_seconds": 45, "native_cases_run": 0}
    save("ready.json", ready)
    print(json.dumps(ready), flush=True)
except Exception as error:
    failure = {"status": "BuildOrPackagingFailure_NoCaseRun", "error": repr(error),
               "original_deadline_epoch": deadline, "remaining_s": deadline - time.time(), "native_cases_run": 0}
    save("failure.json", failure)
    print(json.dumps(failure), flush=True)
    raise
