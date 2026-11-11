"""ROOT seven-target compile gate; no VM, runtime test or benchmark."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import time

source = sys.argv[1]
assert source == "de5e4fbe9c5cbe5796d973b1aff7d50e4644e933"
repo = Path.cwd()
out = Path.home() / "root-managed-events-gpu-seven-de5e-local-v1"
out.mkdir(exist_ok=False)
assert subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip() == source
assert not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"])
rules = hashlib.sha256((repo / "docs/development-rules.md").read_bytes()).hexdigest()
assert rules == "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
assert shutil.disk_usage(out).free >= 30 * 1024**3
(out / "empty-docker").mkdir()
(out / "executed-driver.py").write_bytes(Path(__file__).read_bytes())
assert shutil.disk_usage(out).free >= (30 + 64) * 1024**3
(out / "cargo-home").mkdir()
env = os.environ.copy()
for key in list(env):
    if key.startswith("DYLD_") or key.startswith("SHARDS_"):
        del env[key]
env["CARGO_HOME"] = str(out / "cargo-home")
env["LLVM_BIN"] = "/opt/homebrew/opt/llvm/bin"
assert subprocess.check_output(["zig", "version"], text=True).strip() == "0.16.0"
required = (repo / "resources/supported-targets.txt").read_text().splitlines()
installed = subprocess.check_output(["rustup", "target", "list", "--installed"], text=True).splitlines()
assert set(required) <= set(installed)
assert all(os.access(str(Path(env["LLVM_BIN"]) / tool), os.X_OK) for tool in ["clang-cl", "llvm-lib"])

for key in ["SHARDS_HELPERS_DIR", "SHARDS_INIT_BINARY", "SHARDS_VM_BINARY", "SHARDS_TEST_GUESTS", "SHARDS_SERVER_BINARY"]:
    env.pop(key, None)
env.update(CARGO_BUILD_JOBS="4", CARGO_TARGET_DIR=str(out / "target"), DOCKER_CONFIG=str(out / "empty-docker"))
identity = {"role": "coordinator", "source": source, "rules_sha256": rules,
            "uname": list(os.uname()), "before_free_bytes": shutil.disk_usage(out).free,
            "admission": "4CPU/8GiB/64GiB/j4, not a hard physical/kernel cap",
            "runtime_tests": 0, "virtual_machines": 0, "profiling": False}
for name, args in [("rustc", ["rustc", "--version", "--verbose"]), ("cargo", ["cargo", "--version"])]:
    (out / (name + ".txt")).write_text(subprocess.check_output(args, text=True))
(out / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
command = ["lockf", "/tmp/g121-mac.lock", "sh", "scripts/lint"]
(out / "command.json").write_text(json.dumps(command) + "\n")
began = time.monotonic()
timed_out = False
with (out / "lint.stdout").open("wb") as stdout, (out / "lint.stderr").open("wb") as stderr:
    child = subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
    try:
        code = child.wait(timeout=10200)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(child.pid, signal.SIGKILL)
        code = child.wait(timeout=15)
result = {"exit_code": code, "timeout": timed_out, "direct_child_reaped": child.returncode is not None,
          "elapsed_seconds": time.monotonic() - began, "after_free_bytes": shutil.disk_usage(out).free,
          "whole_population_kernel_return": "Unknown"}
(out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result), flush=True)
if code == 0 and not timed_out:
    output = (out / "lint.stdout").read_text()
    targets = (repo / "resources/supported-targets.txt").read_text().splitlines()
    assert len(targets) == 7 and all("== " + target in output for target in targets)
    assert shutil.disk_usage(out).free >= 30 * 1024**3
    assert not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"])
    (out / "completed.json").write_text(json.dumps({"source": source, "targets": targets,
        "format_and_gpu_variant_checks": "scripts/lint completed0", "runtime_tests": 0,
        "management_layer_complete": False}, indent=2) + "\n")
else:
    for name in ["lint.stdout", "lint.stderr"]:
        print(name, (out / name).read_text(errors="replace")[-4000:], flush=True)
raise SystemExit(124 if timed_out else code)
