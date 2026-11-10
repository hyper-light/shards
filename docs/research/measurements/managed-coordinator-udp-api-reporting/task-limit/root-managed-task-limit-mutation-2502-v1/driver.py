"""Coordinator-only: one bounded excess child in a private real guest."""
from pathlib import Path
import difflib
import hashlib
import json
import os
import shutil
import subprocess
import sys
import traceback

r = Path(sys.argv[1]).resolve()
r.mkdir(exist_ok=False)
shutil.copyfile(__file__, r / "driver.py")
manager = Path("crates/init/src/domains.rs")
actor = Path("crates/testguest/src/managed_limits.rs")
original = {p: p.read_bytes() for p in (manager, actor)}
results = []
errors = []


def run(name, test):
    for p in original:
        p.touch()
    command = ["cargo", "test", "--locked", "-j4", "--profile", "test-release", "-p", "shards",
               "--test", "managed_limits", test, "--", "--ignored", "--exact", "--nocapture",
               "--test-threads=1"]
    (r / (name + ".command.json")).write_text(json.dumps(command) + "\n")
    with (r / (name + ".log")).open("w") as output:
        rc = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT).returncode
    (r / (name + ".exit")).write_text(str(rc) + "\n")
    results.append({"phase": name, "exit": rc})
    print(name, rc, flush=True)
    return rc


def patch(name, path, before, after):
    assert before.count(after) == 0 if after else True
    (r / (name + ".patch")).write_text("".join(difflib.unified_diff(
        before.decode().splitlines(True), after.decode().splitlines(True),
        fromfile=str(path), tofile=str(path))))
    path.write_bytes(after)


recovery = "a_finite_tenant_task_refusal_recovers_without_blocking_its_neighbors"
cancel = "a_finite_held_tenant_population_can_be_cancelled_after_neighbor_progress"
(r / "identity.txt").write_text(str(Path.cwd()) + "\n" + subprocess.check_output(
    ["git", "rev-parse", "HEAD"], text=True) + subprocess.check_output(["rustc", "-Vv"], text=True))
(r / "inputs.json").write_text(json.dumps({k: v for k, v in os.environ.items()
    if k.startswith("MANAGED_LIMIT_") or k in ("SHARDS_TEST_KERNEL", "SHARDS_REQUIRE_VMS",
    "CARGO_TARGET_DIR", "DOCKER_CONFIG")}, indent=2) + "\n")
try:
    assert run("guarded", recovery) == 0
    # TEST-ONLY witness: the existing one excess attempt is released, waited and
    # checked ECHILD before its actual PID is published in the existing failure.
    old = b"                Ok(child) => {\n                    drop(child);\n                    return Err(Failure::UnexpectedAdmission);\n                }"
    new = b"                Ok(child) => {\n                    report.replacement = child.release(until)?;\n                    report.reaped = 1;\n                    return Err(Failure::UnexpectedAdmission);\n                }"
    assert original[actor].count(old) == 1
    instrumented = original[actor].replace(old, new)
    patch("owned-excess-child-witness", actor, original[actor], instrumented)
    old = b'        if let Some(n) = d.pids {\n            std::fs::write(format!("{group}/pids.max"), n.to_string())\n                .map_err(|e| format!("{group}/pids.max: {e}"))?;\n        }'
    assert original[manager].count(old) == 1
    changed = original[manager].replace(old, b"        // Coordinator mutation: omit only the private domain pids.max write.")
    patch("omit-task-limit", manager, original[manager], changed)
    assert run("guard-removed", recovery) == 101
    log = (r / "guard-removed.log").read_text(errors="replace")
    assert "guest fixture failure:" in log and "cause: UnexpectedAdmission" in log, log[-4000:]
    import re
    observed = re.search(r"replacement: ([1-9][0-9]*).*cause: UnexpectedAdmission", log)
    assert observed and "reaped: 1" in log
    (r / "actual-excess.json").write_text(json.dumps({
        "observed_child_pid": int(observed.group(1)), "witness": "owned child ready, normal release, pidfd exit, waitpid and ECHILD before publication",
        "bounded_attempts": "existing finite held children plus exactly one excess",
        "limit": "Other population/host/kernel resource return unobserved"}, indent=2) + "\n")
except BaseException:
    errors.append(traceback.format_exc())
    (r / "campaign-error.txt").write_text(errors[-1])
    print(errors[-1], flush=True)
finally:
    for path, data in original.items():
        path.write_bytes(data)
        path.touch()
        assert path.read_bytes() == data
    (r / "restored-source.sha256.json").write_text(json.dumps({
        str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in original}, indent=2) + "\n")
    (r / "restored.diff").write_bytes(subprocess.check_output(
        ["git", "diff", "--", *(str(p) for p in original)]))
for name, test in (("restored-recovery", recovery), ("restored-held-cancel", cancel)):
    if run(name, test) != 0:
        errors.append(name + " failed")
(r / "results.json").write_text(json.dumps(results, indent=2) + "\n")
(r / "driver.exit").write_text("1\n" if errors else "0\n")
raise SystemExit(1 if errors else 0)
