"""Offline check of retained original VM records; starts no process or VM."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import tarfile

root = Path(__file__).parent
archive = root / "raw.tar.gz"
assert archive.stat().st_size == 79763
assert hashlib.sha256(archive.read_bytes()).hexdigest() == "e4f2776826edf44b8b9bc1f244c682e487af9ffce2e4c3929cbeeb283d69bb71"
files = {}
with tarfile.open(archive) as source:
    for member in source:
        path = PurePosixPath(member.name)
        assert member.isfile() and not path.is_absolute() and ".." not in path.parts
        assert member.name not in files
        files[member.name] = source.extractfile(member).read()
manifest = json.loads(files.pop("ORIGINAL-FILES.json"))
assert len(files) == 111 and set(files) == set(manifest["files"])
for name, data in files.items():
    assert {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()} == manifest["files"][name]


def exported(name):
    document = json.loads(files[name])
    for resource in document["resourceMetrics"]:
        attributes = {row["key"]: row["value"] for row in resource["resource"]["attributes"]}
        if attributes["shards.cgroup.scope"]["stringValue"] == "workload":
            metric = next(row for row in resource["scopeMetrics"][0]["metrics"]
                          if row["name"] == "shards.cgroup.task.events")
            return int(metric["sum"]["dataPoints"][0]["asInt"])
    raise AssertionError("missing workload")


def kernel(name):
    body = files[name].decode().split("== /sys/fs/cgroup/pids.events\n", 1)[1].split("== ", 1)[0]
    return int(dict(line.split() for line in body.splitlines())["max"])


outcomes = []
for stage, expected in [("baseline", 0), ("mutant", 101), ("restored", 0)]:
    result = json.loads(files[stage + "/exact-case.result.json"])
    assert result["exit_code"] == expected and not result["timeout"]
    assert result["pidfd_exit_ready"] and result["direct_child_reaped"]
    assert result["controller_fds_before"] == result["controller_fds_after"]
    prefix = stage + "/events/"
    observed = {"before_kernel": kernel(prefix + "before-kernel.stdout"),
                "after_kernel": kernel(prefix + "after-kernel.stdout"),
                "before_export": exported(prefix + "before.json"),
                "after_export": exported(prefix + "after-refusal.json")}
    assert observed["after_kernel"] > observed["before_kernel"]
    assert b"os error 11" in files[prefix + "refused.stdout"]
    assert b"event-child-ran" not in files[prefix + "refused.stdout"]
    assert b"event-child-ran" in files[prefix + "resumed.stdout"]
    assert b"event-child-stderr" in files[prefix + "resumed.stderr"]
    if stage == "mutant":
        assert observed["before_export"] == observed["after_export"] == 0
        assert b'after_events["task.max"] > before_events["task.max"]' in files[stage + "/exact-case.stderr"]
    else:
        assert observed["after_export"] > observed["before_export"]
        assert b"1 passed; 0 failed; 0 ignored;" in files[stage + "/exact-case.stdout"]
    outcomes.append({"stage": stage, "exit_code": expected, **observed})
original = files["original-events.rs"]
restored = json.loads(files["source-restored.json"])
assert restored["tracked_diff"] == ""
assert restored["events_sha256"] == hashlib.sha256(original).hexdigest()
for stage in ["baseline", "restored"]:
    assert json.loads(files[stage + "/preactor.json"])["events_source_sha256"] == hashlib.sha256(original).hexdigest()
assert json.loads(files["mutant/preactor.json"])["events_source_sha256"] == hashlib.sha256(files["mutant-events.rs"]).hexdigest()
assert len(manifest["frozen_artifacts_verified"]) == 6
print(json.dumps({"original_payloads_verified": len(files), "manifest": 1,
                  "outcomes": outcomes, "source_restored_byte_exact": True,
                  "runtime_replay": False, "whole_population_kernel_return": "Unknown"}, indent=2))
