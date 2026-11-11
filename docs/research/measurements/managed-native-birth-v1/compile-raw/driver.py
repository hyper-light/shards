"""ROOT narrow birth producer on frozen v3; no held audit graph or old-case replay."""
from pathlib import Path
import base64
import hashlib
import json
import os
import resource
import select
import shutil
import signal
import subprocess
import sys
import time
import zlib

began = time.monotonic()
scope = Path.home() / "Projects/root-native-birth-compile-v1"
scope.mkdir(exist_ok=False)
deadline = began + 600
records = scope / "records"
records.mkdir()
spec_bytes = bytes.fromhex(sys.argv[1])
(records / "input.zlib").write_bytes(spec_bytes)
spec = json.loads(zlib.decompress(spec_bytes))
assert os.uname().machine == "x86_64" and os.getuid() != 0
assert shutil.disk_usage(scope).free >= 4 * 1024**3
template = Path.home() / "Projects/managed-native-owner-runtime-01a1c99e-v3"
base_manifest = template / "source/FROZEN-SOURCE.json"
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(base_manifest) == spec["base_manifest_sha256"]
source = scope / "source"
shutil.copytree(template / "source", source, symlinks=True)
for name in ["registry", "git"]:
    shutil.copytree(template / "cargo-home" / name, scope / "cargo-home" / name, symlinks=False)
(scope / "tmp").mkdir()
allowed = sorted(os.sched_getaffinity(0))
assert len(allowed) >= 2
os.sched_setaffinity(0, allowed[:2])
env = os.environ.copy()
assert "LD_PRELOAD" not in env
env.pop("LD_LIBRARY_PATH", None)
env.update(CARGO_HOME=str(scope / "cargo-home"), CARGO_TARGET_DIR=str(scope / "target"),
           CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0", TMPDIR=str(scope / "tmp"))
manifest = json.loads(base_manifest.read_text())
for name, pin in manifest["files"].items():
    p = source / name
    data = os.readlink(p).encode() if p.is_symlink() else p.read_bytes()
    assert len(data) == pin["bytes"] and hashlib.sha256(data).hexdigest() == pin["sha256"], name
(records / "identity.json").write_text(json.dumps({
    "role": "coordinator", "policy": spec["policy"], "uname": list(os.uname()), "uid": os.getuid(),
    "affinity": sorted(os.sched_getaffinity(0)), "source": spec["meaning"],
    "envelope": "normal 2 CPU / 2 GiB / 4 GiB / 600 s including cleanup, j2",
    "physical_kernel_caps": "Unverified", "base_readonly": str(template),
    "forbidden": "No old unit replay, held audit graph/cases, actor/helper/VM/host cgroup writes; no backing claim"
}, indent=2) + "\n")
shutil.copy2(__file__, records / "driver.py")

def limit():
    resource.setrlimit(resource.RLIMIT_AS, (2 * 1024**3, 2 * 1024**3))

def run(name, argv, seconds=180):
    left = deadline - time.monotonic() - 15
    assert left > 0, name
    before = len(list(Path("/proc/self/fd").iterdir()))
    with (records / (name + ".stdout")).open("xb") as out, (records / (name + ".stderr")).open("xb") as err:
        p = subprocess.Popen(argv, cwd=source, env=env, stdout=out, stderr=err,
                             start_new_session=True, preexec_fn=limit)
        pidfd = os.pidfd_open(p.pid)
        poll = select.poll()
        poll.register(pidfd, select.POLLIN)
        ready = bool(poll.poll(int(min(left, seconds) * 1000)))
        if not ready:
            os.killpg(p.pid, signal.SIGKILL)
        code = p.wait(timeout=15)
        os.close(pidfd)
    try:
        second = os.waitpid(p.pid, os.WNOHANG)
    except ChildProcessError:
        second = "ECHILD"
    after = len(list(Path("/proc/self/fd").iterdir()))
    result = {"argv": argv, "exit": code, "pidfd_exit_ready": ready, "direct_child_wait": True,
              "second_wait": second, "fd_before": before, "fd_after": after,
              "elapsed_scope": time.monotonic() - began, "whole_population_kernel_return": "Unknown"}
    (records / (name + ".json")).write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"stage": name, "exit": code, "ready": ready, "fd": [before, after]}), flush=True)
    assert ready and before == after
    return code

code = 1
try:
    patch = records / "caller.patch"
    patch.write_bytes(base64.b64decode(spec["patch_base64"], validate=True))
    assert sha(patch) == spec["patch_sha256"]
    assert run("patch-check", ["git", "apply", "--check", str(patch)]) == 0
    assert run("patch-apply", ["git", "apply", str(patch)]) == 0
    manifest["meaning"] = spec["meaning"]
    manifest["policy"] = spec["policy"]
    manifest["files"].update(spec["files"])
    encoded = json.dumps(manifest, indent=2) + "\n"
    assert hashlib.sha256(encoded.encode()).hexdigest() == spec["expected_manifest_sha256"]
    (source / "FROZEN-SOURCE.json").write_text(encoded)
    for name, pin in manifest["files"].items():
        p = source / name
        data = os.readlink(p).encode() if p.is_symlink() else p.read_bytes()
        assert len(data) == pin["bytes"] and hashlib.sha256(data).hexdigest() == pin["sha256"], name
    # The new module is the only newly authored full file. Existing caller
    # formatting is recorded separately rather than rewriting unrelated code.
    assert run("birth-format", ["rustfmt", "--edition", "2024", "--config", "skip_children=true",
        "crates/init/src/domains/admission/birth.rs"]) == 0
    formatted = source / "crates/init/src/domains/admission/birth.rs"
    shutil.copy2(formatted, records / "birth.rs")
    manifest["files"]["crates/init/src/domains/admission/birth.rs"].update(bytes=formatted.stat().st_size,
                                                                      sha256=sha(formatted))
    (source / "FROZEN-SOURCE.json").write_text(json.dumps(manifest, indent=2) + "\n")
    frozen_manifest = sha(source / "FROZEN-SOURCE.json")
    (records / "source-before.json").write_text(json.dumps({"manifest": frozen_manifest,
        "files": len(manifest["files"]), "changed": spec["files"], "birth_formatted_sha256": sha(formatted)}, indent=2) + "\n")
    assert run("policy", ["python3", "scripts/check-development-policy"]) == 0
    assert run("full-init-check", ["cargo", "check", "--locked", "-p", "shards-init", "--all-targets",
        "--target", "x86_64-unknown-linux-gnu", "-j2"]) == 0
    assert run("test-compile", ["cargo", "test", "--locked", "-p", "shards-init", "--bin", "shards-init",
        "--target", "x86_64-unknown-linux-gnu", "-j2", "--no-run", "--message-format=json"]) == 0
    binaries = []
    for line in (records / "test-compile.stdout").read_text().splitlines():
        item = json.loads(line)
        if item.get("reason") == "compiler-artifact" and item.get("profile", {}).get("test") and item.get("executable"):
            binaries.append(Path(item["executable"]))
    assert len(binaries) == 1, binaries
    original = binaries[0]
    binary = scope / "frozen-test"
    shutil.copy2(original, binary)
    binary_sha = sha(binary)
    (records / "precase-binary.json").write_text(json.dumps({"sha256": binary_sha, "bytes": binary.stat().st_size,
        "source_manifest": frozen_manifest}, indent=2) + "\n")
    assert run("test-list", [str(binary), "--list", "--format", "terse"]) == 0
    names = ["domains::admission::birth::tests::only_regular_owner_write_without_other_writers_is_accepted",
             "domains::admission::birth::tests::binding_and_filesystem_refusals_precede_any_inode_change",
             "managed::native::jobs::tests::native_owner_process_requires_original_birth_access"]
    listed = (records / "test-list.stdout").read_text().splitlines()
    for name in names:
        assert listed.count(name + ": test") == 1, name
    for index, name in enumerate(names):
        stage = "birth-case-" + str(index)
        assert run(stage, [str(binary), name, "--exact", "--nocapture", "--test-threads=1"], 30) == 0
        text = (records / (stage + ".stdout")).read_text()
        assert "1 passed; 0 failed; 0 ignored;" in text
    assert sha(binary) == binary_sha and sha(original) == binary_sha
    code = run("full-init-strict", ["cargo", "clippy", "--locked", "-p", "shards-init", "--all-targets",
        "--target", "x86_64-unknown-linux-gnu", "-j2", "--", "-D", "warnings"])
    for name, pin in manifest["files"].items():
        p = source / name
        data = os.readlink(p).encode() if p.is_symlink() else p.read_bytes()
        assert len(data) == pin["bytes"] and hashlib.sha256(data).hexdigest() == pin["sha256"], name
finally:
    (records / "final.json").write_text(json.dumps({"exit": code, "elapsed": time.monotonic() - began,
        "source": spec["meaning"], "held_graph_executed": False, "backing": "Unverified"}, indent=2) + "\n")
    pins = {p.name: {"bytes": p.stat().st_size, "sha256": sha(p)} for p in sorted(records.iterdir()) if p.is_file()}
    (records / "MANIFEST.json").write_text(json.dumps(pins, indent=2) + "\n")
sys.exit(code)
