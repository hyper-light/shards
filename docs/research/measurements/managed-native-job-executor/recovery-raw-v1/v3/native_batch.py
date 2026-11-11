"""One ordinary pinned leaf build; native cases require the frozen build receipt."""
import hashlib
import json
import math
import os
from pathlib import Path
import resource
import select
import shutil
import signal
import subprocess
import sys
import time
import traceback

ROOT = Path(__file__).resolve().parent
SOURCE = ROOT / "source"
RECORD = ROOT / "record"
ARTIFACTS = ROOT / "artifacts"
GNU = "x86_64-unknown-linux-gnu"
PURE = "native_effects::process::tests::denied_literal_request_and_script_access_preserve_objects_before_birth"
ORDINARY = "native_effects::process::tests::ordinary_literal_job_has_exact_bytes_environment_workdir_and_owned_reap"

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def fds():
    return len(os.listdir("/proc/self/fd"))

def remaining():
    return budget["deadline_monotonic_ns"] - time.monotonic_ns()

def emit(value, name=None):
    if name:
        (RECORD / (name + ".json")).write_text(json.dumps(value, indent=2) + "\n")
    print(json.dumps(value), flush=True)

def source_check():
    manifest = json.loads((SOURCE / "SOURCE-MANIFEST.json").read_text())
    for entry in manifest["files"]:
        path = SOURCE / entry["path"]
        if path.stat().st_size != entry["bytes"] or sha(path) != entry["sha256"]:
            raise RuntimeError("source identity mismatch: " + entry["path"])
    return sha(SOURCE / "SOURCE-MANIFEST.json")

def phase(name, command, case_seconds=None):
    before = fds()
    start = time.monotonic_ns()
    timeout = remaining() / 1_000_000_000
    if case_seconds is not None:
        timeout = min(timeout, case_seconds)
    if timeout <= 0:
        raise TimeoutError("original total budget exhausted before " + name)
    with (RECORD / (name + ".stdout")).open("wb") as out, (RECORD / (name + ".stderr")).open("wb") as err:
        child = subprocess.Popen(command, cwd=SOURCE, env=environment, stdout=out, stderr=err, start_new_session=True)
        held = os.pidfd_open(child.pid)
        waiter = select.poll()
        waiter.register(held, select.POLLIN)
        events = waiter.poll(math.ceil(timeout * 1000))
        ready = any(fd == held and revents & select.POLLIN for fd, revents in events)
        forced = not ready
        if forced:
            os.killpg(child.pid, signal.SIGKILL)
        status = child.wait()
        os.close(held)
    result = {"phase": name, "command": command, "pid": child.pid, "status": status,
              "pidfd_exit_ready": ready, "direct_reaped": child.returncode is not None,
              "forced_owned_cleanup": forced, "fd_before": before, "fd_after": fds(),
              "elapsed_ns": time.monotonic_ns() - start, "original_remaining_ns": remaining()}
    emit(result, name)
    if status != 0 or not ready or result["fd_before"] != result["fd_after"]:
        raise RuntimeError("phase failed: " + name)

def build():
    phase("toolchain", ["rustc", "-vV"])
    if not (RECORD / "toolchain.stdout").read_text().startswith("rustc 1.98.1 "):
        raise RuntimeError("existing compiler is not repository-pinned 1.98.1")
    common = ["--locked", "--target", GNU, "-j2"]
    phase("format", ["rustfmt", "--check", "--edition", "2024", "src/native_effects/process.rs", "child_fixture.rs"])
    phase("check", ["cargo", "check", "--tests", *common])
    phase("clippy", ["cargo", "clippy", "--tests", *common, "--", "-Dwarnings"])
    phase("fixture-build", ["rustc", "--edition", "2024", "-Ccodegen-units=2", "child_fixture.rs", "-o", str(ARTIFACTS / "child-fixture")])
    phase("test-build", ["cargo", "test", "--lib", "--no-run", *common, "--message-format=json"])
    candidates = []
    for line in (RECORD / "test-build.stdout").read_text().splitlines():
        try:
            artifact = json.loads(line)
        except json.JSONDecodeError:
            continue
        if artifact.get("reason") == "compiler-artifact" and artifact.get("profile", {}).get("test") and artifact.get("executable"):
            candidates.append(Path(artifact["executable"]))
    if len(candidates) != 1:
        raise RuntimeError("expected one exact test executable")
    binary = ARTIFACTS / "native-gnu"
    shutil.copy2(candidates[0], binary)
    phase("list-pure", [str(binary), "--list", "--exact", PURE])
    phase("list-ordinary", [str(binary), "--list", "--ignored", "--exact", ORDINARY])
    for name, exact in [("list-pure", PURE), ("list-ordinary", ORDINARY)]:
        if (RECORD / (name + ".stdout")).read_text() != exact + ": test\n\n1 test, 0 benchmarks\n":
            raise RuntimeError("exact filter did not list one case")
    emit({"state": "READY_NATIVE_UNRUN", "native_cases_started": 0,
          "source_manifest_sha256": source_check(),
          "process_sha256": sha(SOURCE / "src/native_effects/process.rs"),
          "tests_sha256": sha(SOURCE / "src/native_effects/process/tests.rs"),
          "binary_sha256": sha(binary), "binary_bytes": binary.stat().st_size,
          "fixture_sha256": sha(ARTIFACTS / "child-fixture"),
          "original_remaining_ns": remaining(), "affinity": sorted(os.sched_getaffinity(0)),
          "rlimit_as": resource.getrlimit(resource.RLIMIT_AS), "physical_kernel_caps": "Unverified",
          "consumer": "leaf projection; real helper/owner integration remains Native/ROOT"}, "ready-native-unrun")

def native():
    frozen = json.loads((RECORD / "ready-native-unrun.json").read_text())
    binary = ARTIFACTS / "native-gnu"
    if sha(binary) != frozen["binary_sha256"] or sha(ARTIFACTS / "child-fixture") != frozen["fixture_sha256"]:
        raise RuntimeError("frozen binary changed")
    if (RECORD / "ordinary.json").exists() or (RECORD / "pure.json").exists():
        raise RuntimeError("executed cases cannot be restarted")
    environment["SHARDS_JOB_FIXTURE_BINARY"] = str(ARTIFACTS / "child-fixture")
    phase("pure", [str(binary), "--exact", PURE, "--nocapture", "--test-threads=1"], 30)
    phase("ordinary", [str(binary), "--exact", ORDINARY, "--ignored", "--nocapture", "--test-threads=1"], 30)
    emit({"state": "COMPLETED", "first_pure_exit": 0, "first_ordinary_exit": 0,
          "source_manifest_sha256": source_check(), "binary_sha256_after": sha(binary),
          "fixture_sha256_after": sha(ARTIFACTS / "child-fixture"), "fd_after": fds(),
          "original_remaining_ns": remaining(), "kernel_population_return": "Unknown",
          "owner_admission": "Not exercised by mechanics-only fixture"}, "result")

if __name__ == "__main__":
    mode = sys.argv[1]
    RECORD.mkdir(exist_ok=True)
    ARTIFACTS.mkdir(exist_ok=True)
    (ROOT / "tmp").mkdir(exist_ok=True)
    budget_file = ROOT / "budget.json"
    if mode == "build":
        started = time.monotonic_ns()
        with budget_file.open("x") as out:
            json.dump({"started_monotonic_ns": started, "deadline_monotonic_ns": started + 600_000_000_000}, out)
    budget = json.loads(budget_file.read_text())
    resource.setrlimit(resource.RLIMIT_AS, (2 * 1024**3, 2 * 1024**3))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:2])
    environment = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target"), CARGO_HOME=str(ROOT / "cargo-home"),
                       TMPDIR=str(ROOT / "tmp"), CARGO_BUILD_JOBS="2", RUSTUP_TOOLCHAIN="1.98.1")
    try:
        source_check()
        if mode in ("build", "build-inherited"):
            build()
        elif mode == "native":
            native()
        else:
            raise ValueError("unknown exact phase")
    except BaseException as error:
        emit({"state": "FAILED", "phase": mode, "error": {"type": type(error).__name__, "detail": str(error)},
              "traceback": traceback.format_exc(), "fd_after": fds(), "original_remaining_ns": remaining(),
              "kernel_population_return": "Unknown"}, "failure-" + mode)
        raise
