"""ROOT-only data-integrity mutation; three fresh normal carried builds."""
from pathlib import Path
import hashlib
import json
import os
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time


REVISION = "a8ad1ce8718bb08d8acaffce7c85e25ce49bce49"
CASE = "metrics::task_refusals_export_the_actual_kernel_event_counter"
RULES = "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
ROOT = Path.home() / "root-managed-events-integrity-a8ad-v1"
ROOT.mkdir(exist_ok=False)
DEADLINE = time.monotonic() + 7200
REPO = ROOT / "source"
subprocess.run(["git", "worktree", "add", "--detach", str(REPO), REVISION], check=True)
assert hashlib.sha256((REPO / "docs/development-rules.md").read_bytes()).hexdigest() == RULES
assert os.access("/dev/kvm", os.R_OK | os.W_OK)
(ROOT / "executed-driver.py").write_bytes(Path(__file__).read_bytes())
(ROOT / "empty-docker").mkdir()
(ROOT / "cargo-home").mkdir()
TEMP = Path(tempfile.mkdtemp(prefix="mi-", dir="/tmp"))
ENV = os.environ.copy()
for key in list(ENV):
    if key.startswith("SHARDS_"):
        del ENV[key]
ENV.update(CARGO_BUILD_JOBS="4", CARGO_HOME=str(ROOT / "cargo-home"),
           DOCKER_CONFIG=str(ROOT / "empty-docker"), TMPDIR=str(TEMP))
(ROOT / "identity.json").write_text(json.dumps({
    "role": "coordinator", "source": REVISION, "rules_sha256": RULES,
    "admission": "4CPU/16GiB/64GiB/j4/7200s; not a kernel-backed resource promise",
    "deadline_monotonic": DEADLINE, "uname": list(os.uname()),
    "original_records": "unchanged r40 originals; fresh build targets; read-only exact pinned kernel reused",
    "temporary_root": str(TEMP), "mutation_scope": "task event observation only",
    "held_campaigns": "untouched", "before_free_bytes": shutil.disk_usage(ROOT).free,
}, indent=2) + "\n")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(directory, name, argv, seconds, environment):
    # Keep the existing15-second direct-child reap bound within the one total budget.
    allowance = min(seconds, DEADLINE - time.monotonic() - 15)
    assert allowance > 0
    before = sorted(os.listdir("/proc/self/fd"))
    (directory / (name + ".command.json")).write_text(json.dumps({
        "argv": argv, "cwd": str(REPO), "timeout_s": allowance,
    }, indent=2) + "\n")
    began = time.monotonic()
    timed = False
    ready = False
    with (directory / (name + ".stdout")).open("xb") as so, (directory / (name + ".stderr")).open("xb") as se:
        child = subprocess.Popen(argv, cwd=REPO, env=environment, stdout=so, stderr=se,
                                 start_new_session=True)
        pidfd = os.pidfd_open(child.pid, 0)
        try:
            event = select.poll()
            event.register(pidfd, select.POLLIN)
            ready = bool(event.poll(max(0, int(allowance * 1000))))
            if not ready:
                timed = True
                os.killpg(child.pid, signal.SIGKILL)
            code = child.wait(timeout=15)
        finally:
            os.close(pidfd)
    record = {"exit_code": code, "timeout": timed, "elapsed_seconds": time.monotonic() - began,
              "pidfd_exit_ready": ready, "direct_child_reaped": child.returncode is not None,
              "controller_fds_before": before, "controller_fds_after": sorted(os.listdir("/proc/self/fd")),
              "whole_population_kernel_return": "Unknown"}
    (directory / (name + ".result.json")).write_text(json.dumps(record, indent=2) + "\n")
    print(name, json.dumps(record), flush=True)
    assert record["controller_fds_before"] == record["controller_fds_after"]
    return record


def task_event(path):
    document = json.loads(path.read_bytes())
    for group in document["resourceMetrics"]:
        attrs = {row["key"]: row["value"] for row in group["resource"]["attributes"]}
        if attrs["shards.cgroup.scope"]["stringValue"] == "workload":
            metric = next(row for row in group["scopeMetrics"][0]["metrics"]
                          if row["name"] == "shards.cgroup.task.events")
            return int(metric["sum"]["dataPoints"][0]["asInt"])
    raise AssertionError("workload observation absent")


def kernel_event(path):
    text = path.read_text().split("== /sys/fs/cgroup/pids.events\n", 1)[1].split("== ", 1)[0]
    return int(dict(line.split() for line in text.splitlines())["max"])


events = REPO / "crates/init/src/metrics/events.rs"
original = events.read_bytes()
needle = b"Token::Word(word) => number(word),"
assert original.count(needle) == 1
replacement = b"Token::Word(word) => if CgroupEvent::ALL.get(*at) == Some(&CgroupEvent::TaskMax) { Ok(0) } else { number(word) },"
mutant = original.replace(needle, replacement)
(ROOT / "original-events.rs").write_bytes(original)
(ROOT / "mutant-events.rs").write_bytes(mutant)
(ROOT / "mutation.json").write_text(json.dumps({
    "path": str(events.relative_to(REPO)), "before_sha256": hashlib.sha256(original).hexdigest(),
    "mutant_sha256": hashlib.sha256(mutant).hexdigest(),
    "effect": "corrupt a parsed task.max observation to zero; kernel enforcement remains unchanged",
    "production_or_main_modified": False, "stages": ["baseline", "mutant", "restored"],
}, indent=2) + "\n")
kernel = Path.home() / "root-managed-events-a8ad1ce8718b-v1/target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"
assert digest(kernel) == "48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
guest = None
outcomes = []
try:
    for stage in ["baseline", "mutant", "restored"]:
        events.write_bytes(mutant if stage == "mutant" else original)
        directory = ROOT / stage
        directory.mkdir()
        env = ENV | {"CARGO_TARGET_DIR": str(directory / "target")}
        assert run(directory, "normal-carried-build", ["cargo", "test", "--locked", "-p", "shards", "--profile",
                   "test-release", "--test", "managed_output", "--no-run", "--message-format=json", "-j4"], 2400, env)["exit_code"] == 0
        artifacts = []
        executables = set()
        for line in (directory / "normal-carried-build.stdout").read_text().splitlines():
            try:
                item = json.loads(line)
            except json.JSONDecodeError:
                continue
            if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "managed_output" and item.get("executable"):
                artifacts.append(Path(item["executable"]))
            if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "shards" and item.get("executable"):
                executables.add(Path(item["executable"]))
        assert len(artifacts) == 1
        assert len(executables) == 1
        binary = directory / "managed-output-frozen"
        shutil.copy2(artifacts[0], binary)
        carried = directory / "shards-carried-frozen"
        shutil.copy2(next(iter(executables)), carried)
        if guest is None:
            guest_env = env | {"CC_x86_64_unknown_linux_musl": str(REPO / "scripts/zig-cc"),
                               "AR_x86_64_unknown_linux_musl": str(REPO / "scripts/zig-ar"), "ZIG_TARGET": "x86_64-linux-musl"}
            assert run(directory, "matching-static-guest", ["cargo", "build", "--locked", "-p", "shards-testguest", "--profile",
                       "guest-test", "--target", "x86_64-unknown-linux-musl", "-j4"], 900, guest_env)["exit_code"] == 0
            prefab = ROOT / "guests/guest"
            prefab.mkdir(parents=True)
            guest = prefab / "shards-testguest"
            shutil.copy2(directory / "target/x86_64-unknown-linux-musl/guest-test/shards-testguest", guest)
        native = env | {"SHARDS_REQUIRE_VMS": "1", "SHARDS_TEST_GUESTS": str(ROOT / "guests"),
                        "SHARDS_TEST_KERNEL": str(kernel), "SHARDS_OTEL_EVENTS_RECORDS": str(directory / "events"),
                        "XDG_CACHE_HOME": str(directory / "runtime-cache"), "SHARDS_CACHE": str(directory / "runtime-cache/shards")}
        listed = run(directory, "exact-list", [str(binary), CASE, "--exact", "--list"], 60, native)
        assert listed["exit_code"] == 0 and (directory / "exact-list.stdout").read_text().count(": test\n") == 1
        (directory / "preactor.json").write_text(json.dumps({
            "stage": stage, "source": REVISION, "events_source_sha256": digest(events),
            "binary_sha256": digest(binary), "binary_bytes": binary.stat().st_size,
            "carried_shards_sha256": digest(carried),
            "guest_sha256": digest(guest), "kernel_sha256": digest(kernel), "selected_count": 1,
        }, indent=2) + "\n")
        result = run(directory, "exact-case", [str(binary), CASE, "--exact", "--nocapture", "--test-threads=1"], 1200, native)
        facts = {"before_export": task_event(directory / "events/before.json"),
                 "after_export": task_event(directory / "events/after-refusal.json"),
                 "before_kernel": kernel_event(directory / "events/before-kernel.stdout"),
                 "after_kernel": kernel_event(directory / "events/after-kernel.stdout")}
        assert facts["after_kernel"] > facts["before_kernel"]
        assert "os error 11" in (directory / "events/refused.stdout").read_text()
        assert "event-child-ran" in (directory / "events/resumed.stdout").read_text()
        if stage == "mutant":
            assert result["exit_code"] == 101 and not result["timeout"]
            assert facts["before_export"] == facts["after_export"] == 0
            assert 'after_events["task.max"] > before_events["task.max"]' in (directory / "exact-case.stderr").read_text()
        else:
            assert result["exit_code"] == 0 and not result["timeout"]
            assert facts["after_export"] > facts["before_export"]
            assert "1 passed; 0 failed; 0 ignored;" in (directory / "exact-case.stdout").read_text()
        (directory / "observed.json").write_text(json.dumps(facts, indent=2) + "\n")
        outcomes.append({"stage": stage, **result, **facts})
finally:
    events.write_bytes(original)
    assert events.read_bytes() == original
    (ROOT / "source-restored.json").write_text(json.dumps({
        "events_sha256": digest(events), "tracked_diff": subprocess.check_output(["git", "diff", "--stat"], cwd=REPO, text=True),
    }, indent=2) + "\n")
assert len(outcomes) == 3
assert not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=REPO)
(ROOT / "completed.json").write_text(json.dumps({"source": REVISION, "outcomes": outcomes,
    "counter_corruption_observed_and_detected": True, "source_restored_byte_exact": True,
    "all_three_binaries_preserved": True, "whole_population_kernel_return": "Unknown",
    "full_management_complete": False}, indent=2) + "\n")
print("COMPLETE", str(ROOT), flush=True)
