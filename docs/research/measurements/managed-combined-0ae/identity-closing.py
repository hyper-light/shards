"""Coordinator: current-source OTLP acceptance, identity mutation and encoding costs."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import signal
import subprocess
import time

SOURCE = "0ae89529036e4e30dba639169844f1bcb5ba84da"
POLICY = "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
h = Path.home()
repo = h / "managed-capture-check"
out = h / "root-managed-identity-closing-0ae-v1"
out.mkdir(exist_ok=False)
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
assert subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip() == SOURCE
assert not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo)
assert sha(repo / "docs/development-rules.md") == POLICY
env = os.environ.copy()
for key in ["SHARDS_HELPERS", "SHARDS_HELPERS_DIR", "SHARDS_INIT_BINARY", "SHARDS_VM_BINARY", "SHARDS_TEST_GUESTS", "SHARDS_SERVER_BINARY", "SHARDS_TEST_KERNEL", "SHARDS_KERNEL", "SHARDS_INIT"]:
    env.pop(key, None)
env.update(CARGO_BUILD_JOBS="4", CARGO_TARGET_DIR=str(repo / "target"))
for name in ["empty-docker", "tmp", "reference", "profile"]:
    (out / name).mkdir()
env["DOCKER_CONFIG"] = str(out / "empty-docker")

def run(name, args, seconds=300, used_env=None, stdout=None):
    destination = stdout or out / (name + ".log")
    (out / (name + ".command.json")).write_text(json.dumps(args) + "\n")
    start = time.monotonic()
    with destination.open("wb") as log:
        p = subprocess.Popen(args, cwd=repo, env=used_env or env, stdout=log, stderr=subprocess.PIPE, start_new_session=True)
        try:
            error = p.communicate(timeout=seconds)[1]
            rc, timed = p.returncode, False
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            error = p.communicate(timeout=15)[1]
            rc, timed = 124, True
    (out / (name + ".stderr")).write_bytes(error)
    result = {"exit_code": rc, "timeout": timed, "elapsed_seconds": time.monotonic() - start}
    (out / (name + ".result.json")).write_text(json.dumps(result, indent=2) + "\n")
    print(name, json.dumps(result), flush=True)
    if rc:
        print(error.decode(errors="replace")[-4000:], flush=True)
    return rc

identity = {"source": SOURCE, "policy_sha256": POLICY, "role": "coordinator", "uname": list(os.uname()), "load_before": os.getloadavg(), "admission": "4CPU/8GiB/16GiB; admission is not a hard cgroup or measured high-water bound"}
for name in ["crates/init/src/metrics/otlp.rs", "crates/shards/src/daemon/metrics.rs", "docs/research/measurements/managed-otel-rust-reference/src/reference.rs", "docs/research/measurements/managed-otel/profile/src/main.rs", "docs/research/measurements/managed-otel/profile/src/allocations.rs"]:
    identity[name] = sha(repo / name)
(out / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
oracle = h / "root-managed-otel-oracle-fedora-28e-v1/oracle"
collector = h / "root-managed-otel-collector-v0165-v1/otelcol"
assert sha(oracle) == "dc8281fbaa3f6e6b325958f706c31425d3f2f7783c639ebd2bce24c46dfee62b"
assert sha(collector) == "14e254a6a0880d9b6f94487658e49fc9f3b0e14c62e82253be78da08e8c1ddd8"
actual = h / "root-managed-combined-0ae-v2"
inputs = {"actual-first": actual / "ordinary-metrics/first.json", "actual-second": actual / "ordinary-metrics/second.json", "actual-provider": actual / "ordinary-joiner/provider.json", "actual-joiner": actual / "ordinary-joiner/joiner.json"}
receipts = []
for name, path in inputs.items():
    target = out / (name + ".json")
    shutil.copy2(path, target)
    assert run(name + "-decode", [str(oracle), "-max-bytes", str(target.stat().st_size), "-input", str(target)], stdout=out / (name + ".receipt.json")) == 0
    receipt = json.loads((out / (name + ".receipt.json")).read_bytes())
    receipts.append({"name": name, "resources": len(receipt["resources"]), "input_sha256": sha(target)})
    assert run(name + "-collector", ["python3", str(repo / "docs/research/measurements/managed-otel/collector-http.py"), "--collector", str(collector), "--input", str(target), "--out", str(out / (name + "-collector")), "--collector-sha256", sha(collector), "--startup-seconds", "10", "--request-seconds", "10", "--cleanup-seconds", "15"], 60) == 0
assert [r["resources"] for r in receipts] == [4, 4, 4, 1]
(out / "actual-acceptance.json").write_text(json.dumps(receipts, indent=2) + "\n")

reference = repo / "docs/research/measurements/managed-otel-rust-reference"
profiling = repo / "docs/research/measurements/managed-otel/profile"
profile_env = env | {"CARGO_TARGET_DIR": str(h / "root-managed-otel-profile-cf8-v1/target"), "CARGO_BUILD_JOBS": "2"}
assert run("reference-test", ["cargo", "test", "--locked", "--manifest-path", str(reference / "Cargo.toml"), "-j2"], 600, profile_env) == 0
assert run("reference-build", ["cargo", "build", "--locked", "--release", "--manifest-path", str(reference / "Cargo.toml"), "-j2"], 600, profile_env) == 0
ref_binary = Path(profile_env["CARGO_TARGET_DIR"]) / "release/shards-managed-otel-rust-reference"
modes = ["native-stream", "official-compact-buffer", "official-pretty-buffer"]
decoded = []
for mode in modes:
    payload = out / "reference" / (mode + ".json")
    assert run("reference-" + mode, [str(ref_binary), mode], stdout=payload) == 0
    receipt = out / "reference" / (mode + ".receipt.json")
    assert run("reference-decode-" + mode, [str(oracle), "-max-bytes", str(payload.stat().st_size), "-input", str(payload)], stdout=receipt) == 0
    decoded.append(json.loads(receipt.read_bytes()))
assert decoded[0] == decoded[1] == decoded[2]
assert run("reference-shape", ["python3", str(reference / "check.py"), str(out / "reference")]) == 0
common = ["cargo", "build", "--locked", "--release", "--manifest-path", str(profiling / "Cargo.toml"), "-j2"]
assert run("profile-timing-build", common, 600, profile_env) == 0
timing = out / "profile/uninstrumented"
shutil.copy2(Path(profile_env["CARGO_TARGET_DIR"]) / "release/shards-managed-otel-profile", timing)
assert run("profile-count-build", common + ["--features", "alloc-count"], 600, profile_env) == 0
count = out / "profile/allocation-count"
shutil.copy2(Path(profile_env["CARGO_TARGET_DIR"]) / "release/shards-managed-otel-profile", count)
assert run("profile-clippy", ["cargo", "clippy", "--locked", "--manifest-path", str(profiling / "Cargo.toml"), "--all-targets", "--all-features", "-j2", "--", "-D", "warnings"], 600, profile_env) == 0
for block in range(9):
    for mode in modes[block % 3:] + modes[:block % 3]:
        name = "timing-" + str(block) + "-" + mode
        assert run(name, [str(timing), mode, "1000"], 30, stdout=out / "profile" / (name + ".json")) == 0
for mode in modes:
    name = "allocations-" + mode
    path = out / "profile" / (name + ".json")
    assert run(name, [str(count), mode, "100"], 30, stdout=path) == 0
    allocations = json.loads(path.read_bytes())["allocations"]
    assert allocations["live_before"] == allocations["live_after"], allocations
(out / "profile/binaries.json").write_text(json.dumps({"uninstrumented": sha(timing), "allocation_count": sha(count), "samples_per_mode": 9000, "blocks": 9, "allocation_calls_per_mode": 100, "scope": "encoding only, Count Write; excludes collection/real transport/VM/SDK pipeline/parent+kernel; historical source results are not reused"}, indent=2) + "\n")

file = repo / "crates/shards/src/daemon/metrics.rs"
original = file.read_bytes()
needle = b"let microvm = run.joined.as_deref().unwrap_or(&id);"
replacement = b"let _ = run;\n            let microvm = id.as_str();"
assert original.count(needle) == 1
(out / "before-metrics.rs").write_bytes(original)
kernel = repo / "target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"
assert sha(kernel) == "48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
def identity_case(label, expected):
    assert run(label + "-normal-build", ["cargo", "test", "--locked", "-p", "shards", "--profile", "test-release", "--test", "managed_output", "--no-run", "--message-format=json", "-j4"], 1800) == 0
    binaries = []
    for line in (out / (label + "-normal-build.log")).read_text().splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "managed_output" and item.get("executable"):
            binaries.append(Path(item["executable"]))
    assert len(binaries) == 1
    binary = binaries[0]
    shutil.copy2(binary, out / (label + "-test"))
    used = env | {"SHARDS_REQUIRE_VMS": "1", "SHARDS_TEST_GUESTS": str(actual / "guests"), "SHARDS_TEST_KERNEL": str(kernel), "TMPDIR": str(out / "tmp"), "XDG_CACHE_HOME": str(out / (label + "-runtime-cache")), "SHARDS_CACHE": str(out / (label + "-runtime-cache/shards")), "SHARDS_OTEL_JOINER_RECORDS": str(out / (label + "-records"))}
    rc = run(label + "-kvm", [str(binary), "metrics::a_joiners_metrics_distinguish_its_run_from_the_shared_microvm", "--exact", "--nocapture", "--test-threads=1"], 300, used)
    text = (out / (label + "-kvm.log")).read_text(errors="replace")
    assert "SKIP:" not in text and "running 1 test" in text
    records = out / (label + "-records")
    provider, joiner = [(records / (name + ".id")).read_text() for name in ["provider", "joiner"]]
    document = json.loads((records / "joiner.json").read_bytes())
    resources = document["resourceMetrics"]
    assert len(resources) == 1
    attrs = {a["key"]: a["value"] for a in resources[0]["resource"]["attributes"]}
    vm = attrs["shards.microvm.id"]["stringValue"]
    assert attrs["shards.run.id"]["stringValue"] == joiner and provider != joiner
    if expected:
        assert rc == 0 and vm == provider and "1 passed; 0 failed; 0 ignored;" in text
    else:
        assert rc == 101 and vm == joiner and "assertion" in text
    return {"exit_code": rc, "run": joiner, "actual_microvm": vm, "provider": provider, "test_sha256": sha(binary), "source_sha256": sha(file), "export_sha256": sha(records / "joiner.json")}

try:
    file.write_bytes(original.replace(needle, replacement))
    (out / "identity-mutation.diff").write_bytes(subprocess.check_output(["git", "diff", "--", str(file.relative_to(repo))], cwd=repo))
    mutated = identity_case("mutated", False)
finally:
    file.write_bytes(original)
    assert sha(file) == hashlib.sha256(original).hexdigest()
    (out / "restored.sha256").write_text(sha(file) + "\n")
restored = identity_case("restored", True)
assert not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo)
(out / "completed.json").write_text(json.dumps({"source": SOURCE, "mutated": mutated, "restored": restored, "actual_decoded_inputs": receipts, "reference_receipts_equal": True, "load_after": os.getloadavg(), "time_unix_ns": time.time_ns(), "scope": "source-specific identity/caller/conformance and encoding gates; full management plan remains open"}, indent=2) + "\n")
print("COMPLETE", SOURCE, str(out), flush=True)
