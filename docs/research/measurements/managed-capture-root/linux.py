"""Coordinator: current carried caller, real cap transitions and official consumers."""
from pathlib import Path
import hashlib
import json
import os
import select
import shutil
import signal
import subprocess
import sys
import time

source = sys.argv[1]
assert len(source) == 40 and all(c in "0123456789abcdef" for c in source)
home = Path.home()
repo = home / "root-managed-capture-7fe-v1/source"
out = home / ("root-managed-capture-" + source[:12] + "-v1")
out.mkdir(exist_ok=False)
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
env = os.environ.copy()
for key in ["SHARDS_HELPERS", "SHARDS_HELPERS_DIR", "SHARDS_INIT_BINARY", "SHARDS_VM_BINARY", "SHARDS_TEST_GUESTS", "SHARDS_SERVER_BINARY", "SHARDS_TEST_KERNEL", "SHARDS_KERNEL", "SHARDS_INIT"]:
    env.pop(key, None)
for name in ["empty-docker", "tmp", "frozen"]:
    (out / name).mkdir()
env.update(CARGO_BUILD_JOBS="4", CARGO_TARGET_DIR=str(repo / "target"), DOCKER_CONFIG=str(out / "empty-docker"))

def run(name, args, seconds, used=None):
    (out / (name + ".command.json")).write_text(json.dumps(args) + "\n")
    began = time.monotonic()
    before = len(list(Path("/proc/self/fd").iterdir()))
    ready = False
    pidfd = None
    timed = False
    with (out / (name + ".stdout")).open("wb") as stdout, (out / (name + ".stderr")).open("wb") as stderr:
        child = subprocess.Popen(args, cwd=repo, env=used or env, stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            pidfd = os.pidfd_open(child.pid, 0)
            poll = select.poll()
            poll.register(pidfd, select.POLLIN)
            ready = bool(poll.poll(seconds * 1000))
            if not ready:
                raise subprocess.TimeoutExpired(args, seconds)
            rc = child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=15)
            rc, timed = 124, True
        finally:
            if pidfd is not None:
                os.close(pidfd)
    result = {"exit_code": rc, "timeout": timed, "elapsed_seconds": time.monotonic() - began, "pidfd_exit_ready": ready, "direct_child_reaped": child.returncode is not None, "controller_fds_before": before, "controller_fds_after": len(list(Path("/proc/self/fd").iterdir())), "whole_population_kernel_return": "Unknown"}
    (out / (name + ".result.json")).write_text(json.dumps(result, indent=2) + "\n")
    print(name, json.dumps(result), flush=True)
    if rc:
        for stream in ["stdout", "stderr"]:
            print(stream, (out / (name + "." + stream)).read_text(errors="replace")[-5000:], flush=True)
    assert result["controller_fds_before"] == result["controller_fds_after"]
    return rc

assert not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo)
assert run("fetch-bundle", ["git", "fetch", str(home / "root-capture-input-v1/source.bundle"), "refs/heads/p/managed-capture-root-v1"], 120) == 0
assert run("checkout", ["git", "checkout", "--detach", source], 120) == 0
assert sha(repo / "docs/development-rules.md") == "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
assert os.access("/dev/kvm", os.R_OK | os.W_OK)
(out / "identity.json").write_text(json.dumps({"role": "coordinator", "source": source, "policy_sha256": sha(repo / "docs/development-rules.md"), "uname": list(os.uname()), "load": os.getloadavg(), "disk_free": shutil.disk_usage(repo).free, "admission": "4CPU/16GiB/64GiB/j4; not a hard physical/kernel cap", "sources": {p: sha(repo / p) for p in ["crates/remote/src/managed.rs", "crates/shards/src/daemon/managed.rs", "crates/init/src/metrics.rs", "crates/init/src/metrics/otlp.rs", "crates/shards/tests/managed_output/metrics.rs", "crates/shards/src/daemon/metrics.rs"]}}, indent=2) + "\n")
assert run("policy", ["python3", "scripts/check-development-policy"], 60) == 0
assert run("metrics-unit", ["cargo", "test", "--locked", "-p", "shards-init", "--profile", "test-release", "-j4", "--", "metrics::", "--nocapture", "--test-threads=1"], 900) == 0
assert run("native-clippy", ["env", "SHARDS_HELPERS=skip", "cargo", "clippy", "--locked", "-p", "shards-init", "-p", "shards", "--all-targets", "-j4", "--", "-D", "warnings"], 1200) == 0
assert run("normal-carried-build", ["cargo", "test", "--locked", "-p", "shards", "--profile", "test-release", "--test", "managed_output", "--no-run", "--message-format=json", "-j4"], 2400) == 0
artifacts = []
for line in (out / "normal-carried-build.stdout").read_text().splitlines():
    try:
        item = json.loads(line)
    except json.JSONDecodeError:
        continue
    if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "managed_output" and item.get("executable"):
        artifacts.append(Path(item["executable"]))
assert len(artifacts) == 1
guest_env = env | {"CC_x86_64_unknown_linux_musl": str(repo / "scripts/zig-cc"), "AR_x86_64_unknown_linux_musl": str(repo / "scripts/zig-ar"), "ZIG_TARGET": "x86_64-linux-musl"}
assert run("matching-static-guest", ["cargo", "build", "--locked", "-p", "shards-testguest", "--profile", "guest-test", "--target", "x86_64-unknown-linux-musl", "-j4"], 900, guest_env) == 0
guest = repo / "target/x86_64-unknown-linux-musl/guest-test/shards-testguest"
prefab = out / "guests/guest"
prefab.mkdir(parents=True)
shutil.copy2(guest, prefab / "shards-testguest")
binary = out / "frozen/managed-output-test"
shutil.copy2(artifacts[0], binary)
kernel = repo / "target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"
if not kernel.exists():
    import urllib.request
    kernel.parent.mkdir(parents=True,exist_ok=True)
    url="https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64"
    with urllib.request.urlopen(url,timeout=120) as response:
        blob=response.read(27758808+1)
    assert len(blob)==27758808
    assert hashlib.sha256(blob).hexdigest()=="48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
    with kernel.open("xb") as handle:handle.write(blob)
assert sha(kernel) == "48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
(out / "artifacts.json").write_text(json.dumps({n: {"path": str(p), "bytes": p.stat().st_size, "sha256": sha(p)} for n, p in [("test", binary), ("guest", guest), ("kernel", kernel)]}, indent=2) + "\n")
native = env | {"SHARDS_REQUIRE_VMS": "1", "SHARDS_TEST_GUESTS": str(out / "guests"), "SHARDS_TEST_KERNEL": str(kernel), "TMPDIR": str(out / "tmp"), "XDG_CACHE_HOME": str(out / "runtime-cache"), "SHARDS_CACHE": str(out / "runtime-cache/shards"), "SHARDS_OTEL_RECORDS": str(out / "metrics"), "SHARDS_OTEL_JOINER_RECORDS": str(out / "joiner")}
assert run("ordinary-managed-output", [str(binary), "--nocapture", "--test-threads=1"], 1200, native) == 0
text = (out / "ordinary-managed-output.stdout").read_text(errors="replace") + (out / "ordinary-managed-output.stderr").read_text(errors="replace")
assert "25 passed; 0 failed; 10 ignored;" in text and "SKIP:" not in text
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
(out/"completed.json").write_text(json.dumps({"source":source,"ordinary_suite":"25passed/0failed/10ignored","official_consumers":"not rerun in this capture batch; codec unchanged from a84, no new acceptance claim","management_layer_complete":False,"time_unix_ns":time.time_ns()},indent=2)+"\n")
print("COMPLETE",source,str(out),flush=True)
