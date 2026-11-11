#!/usr/bin/env python3
"""Copy verified frozen inputs into an exclusive runtime-only scope; no build."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import os
import platform
import subprocess
import sys
import tarfile
import time

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-runtime-v2")
original = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-v1")
start, deadline = map(float, sys.argv[1:3])
assert deadline == start + 600 and start <= time.time() < deadline
assert platform.system() == "Darwin" and platform.machine() == "arm64"
usage = os.statvfs(root.parent)
free = usage.f_bavail * usage.f_frsize
assert free >= 34 * 1024**3
root.mkdir()
for name in ["artifacts", "frozen-inputs", "input", "controller"]:
    (root / name).mkdir()


def save(path, value):
    with path.open("x") as file:
        json.dump(value, file, indent=2)
        file.write("\n")


def sha(data):
    return hashlib.sha256(data).hexdigest()


budget = {"start_epoch": start, "deadline_epoch": deadline, "scope_seconds": 600,
          "runner": "e30189ea29182644fe5a4b2e06c7dac2", "cpus": 2, "memory_bytes": 2 * 1024**3,
          "disk_bytes": 4 * 1024**3, "floor_bytes": 30 * 1024**3, "work_seconds": 30,
          "cleanup_seconds": 15, "parent_seconds": 45, "native_retries_allowed": 0,
          "policy_revision": "749c3ca0602c43dac74976e8db713102f5b15874",
          "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
          "source_revision": "342bcef5855084a58a7257654d88e9917cef016e", "free_bytes_before": free}
save(root / "budget.json", budget)
identities = {}
copies = [("artifacts/shards-vmm-native", "artifacts/shards-vmm-native", "ce4c34d7f480c8b02073d15e0f4ba5b2a11ebfe5b2ceb8bfe2fdc996cc2a8871"),
          ("source-package.tar.gz", "frozen-inputs/source-package.tar.gz", "ba43fd22a2036de11268aa4ae49e2b9ce748b5bee7bbd3fcb615a86237b86b63"),
          ("source-package-manifest.json", "frozen-inputs/source-package-manifest.json", "9a9f9e0c71607179c5d770518cc8f9c21ae55e5a1033090a4452af38bf66aad7"),
          ("input/native-records-v1.tar.gz", "input/native-records-v1.tar.gz", "7b14ede379a4679e4bb3269c40928dda6139358b3fb6342fc4767a4c6404ad46"),
          ("input/corpus-manifest.json", "input/corpus-manifest.json", "804f3b66b34565517f88caf43a1b48ad0102eaede0e15348b548df0a9eaa6698"),
          ("build-records/ready.json", "frozen-inputs/original-ready.json", None)]
for old, new, expected in copies:
    path = original / old
    assert path.is_file() and not path.is_symlink()
    data = path.read_bytes()
    if expected:
        assert sha(data) == expected
    with (root / new).open("xb") as file:
        file.write(data)
    identities[old] = {"path": str(path), "bytes": len(data), "sha256": sha(data), "copied_to": new}
save(root / "frozen-inputs/original-input-identities.json", identities)


def members(archive):
    entries = archive.getmembers()
    assert len({entry.name for entry in entries}) == len(entries)
    for entry in entries:
        path = PurePosixPath(entry.name)
        assert entry.isfile() and str(path) == entry.name and not path.is_absolute()
        assert ".." not in path.parts and "\\" not in entry.name
    return entries


with tarfile.open(root / "frozen-inputs/source-package.tar.gz") as archive:
    entries = members(archive)
    outside = (root / "frozen-inputs/source-package-manifest.json").read_bytes()
    assert archive.extractfile("source-package-manifest.json").read() == outside
    index = json.loads(outside)
    assert index["source_revision"] == budget["source_revision"]
    rows = {row["path"]: row for row in index["files"]}
    assert len(rows) == 176 and set(rows) == {entry.name for entry in entries} - {"source-package-manifest.json"}
    files = {}
    for name, row in rows.items():
        data = archive.extractfile(name).read()
        assert len(data) == row["bytes"] and sha(data) == row["sha256"]
        files[name] = data
    leaf = files["source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"]
    assert sha(leaf) == "65c73f963edef4ffb81754cf2eea2bd4ecd6a44d7f14178994725b211d0a4c65"
    verifier = files["source/docs/research/measurements/managed-audit-wire-adversarial/verify-corpus.py"]
    with (root / "frozen-inputs/verify-corpus.py").open("xb") as file:
        file.write(verifier)
with tarfile.open(root / "input/native-records-v1.tar.gz") as archive:
    entries = members(archive)
    rows = json.loads(archive.extractfile("manifest.json").read())
    index = {row["path"]: row for row in rows}
    assert len(index) == 389 and len(entries) == 390
    assert set(index) == {entry.name for entry in entries} - {"manifest.json"}
    for entry in entries:
        data = archive.extractfile(entry).read()
        if entry.name in index:
            row = index[entry.name]
            assert len(data) == row["bytes"] and sha(data) == row["sha256"]
        target = (root / "input/data" / entry.name.removeprefix("corpus/")) if entry.name.startswith("corpus/") else (root / "frozen-inputs/original-corpus-records" / entry.name)
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("xb") as file:
            file.write(data)
subprocess.run(["python3", str(root / "frozen-inputs/verify-corpus.py"), str(root / "input"), str(root / "corpus-verified.json")], check=True, timeout=min(30, deadline - time.time()))
old_ready = json.loads((root / "frozen-inputs/original-ready.json").read_text())
assert old_ready["sha256"] == identities["artifacts/shards-vmm-native"]["sha256"]
name = "managed_audit_wire_adversarial::reserved_client_disconnect_retains_owned_extent"
assert old_ready["filter"] == name and old_ready["selected_count"] == 1
executable = root / "artifacts/shards-vmm-native"
executable.chmod(0o755)
env = os.environ.copy()
for key in list(env):
    if key.startswith("DYLD_"):
        del env[key]
(root / "list-tmp").mkdir()
env["TMPDIR"] = str(root / "list-tmp")
list_argv = [str(executable), name, "--exact", "--ignored", "--list"]
save(root / "controller/list.command.json", {"argv": list_argv, "timeout_s": min(30, deadline - time.time())})
with (root / "controller/list.stdout").open("xb") as out, (root / "controller/list.stderr").open("xb") as err:
    result = subprocess.run(list_argv, cwd=root, env=env, stdout=out, stderr=err, timeout=min(30, deadline - time.time()))
save(root / "controller/list.result.json", {"exit_code": result.returncode})
assert result.returncode == 0
assert [line for line in (root / "controller/list.stdout").read_text().splitlines() if line.endswith(": test")] == [name + ": test"]
ready = {"status": "FrozenBinaryVerified_ListExactlyOne_NoActor", "at_epoch": time.time(), "budget": budget,
         "executable": str(executable), "sha256": sha(executable.read_bytes()), "filter": name, "selected_count": 1,
         "source_package_sha256": identities["source-package.tar.gz"]["sha256"], "leaf_sha256": sha(leaf),
         "source_payloads_verified": len(files), "original_ready_sha256": identities["build-records/ready.json"]["sha256"],
         "argv": [str(executable), name, "--exact", "--ignored", "--nocapture", "--test-threads=1"],
         "case_environment": {"SHARDS_AUDIT_WIRE_CORPUS": str(root / "input/data"),
                              "SHARDS_AUDIT_WIRE_RECORDS": str(root / "native-records-v2"), "TMPDIR": str(root / "native-tmp-v2")},
         "native_attempts": 0, "no_cargo_or_rebuild": True}
save(root / "controller/ready-v2.json", ready)
print(json.dumps(ready), flush=True)
