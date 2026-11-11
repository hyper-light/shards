#!/usr/bin/env python3
"""New exclusive source/cache/target; normal admitted build only, never actors."""
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

start, deadline = map(float, sys.argv[1:3])
assert deadline == start + 600 and start <= time.time() < deadline
root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-v1")
assert platform.system() == "Darwin" and platform.machine() == "arm64"
usage = os.statvfs(root.parent)
free = usage.f_bavail * usage.f_frsize
assert free >= (30 + 4) * 1024**3, ("30GiB floor plus full new disk ask unavailable", free)
root.mkdir()
budget = {"start_epoch": start, "deadline_epoch": deadline, "scope_seconds": 600,
          "source_revision": "342bcef5855084a58a7257654d88e9917cef016e",
          "runner": "e30189ea29182644fe5a4b2e06c7dac2", "cpus": 2,
          "memory_bytes": 2 * 1024**3, "disk_bytes": 4 * 1024**3, "jobs": 2,
          "build_seconds": 180, "work_seconds": 30, "cleanup_seconds": 15,
          "parent_seconds": 45, "free_bytes_before": free,
          "floor_bytes": 30 * 1024**3, "runtime_retries_allowed": 0}
with (root / "budget.json").open("x") as file:
    json.dump(budget, file, indent=2)
package = Path(__file__).parent / "package-v1/source-package.tar.gz"
raw = package.read_bytes()
assert hashlib.sha256(raw).hexdigest() == "ba43fd22a2036de11268aa4ae49e2b9ce748b5bee7bbd3fcb615a86237b86b63"
with (root / "source-package.tar.gz").open("xb") as file:
    file.write(raw)


def safe(member):
    name = PurePosixPath(member.name)
    assert member.isfile() and str(name) == member.name and not name.is_absolute()
    assert ".." not in name.parts and "\\" not in member.name


with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    members = archive.getmembers()
    assert len(members) == len({member.name for member in members}) == 177
    for member in members:
        safe(member)
    manifest_bytes = archive.extractfile("source-package-manifest.json").read()
    assert hashlib.sha256(manifest_bytes).hexdigest() == "9a9f9e0c71607179c5d770518cc8f9c21ae55e5a1033090a4452af38bf66aad7"
    manifest = json.loads(manifest_bytes)
    rows = {row["path"]: row for row in manifest["files"]}
    assert len(rows) == 176 and manifest["source_revision"] == budget["source_revision"]
    assert set(rows) == {member.name for member in members} - {"source-package-manifest.json"}
    for member in members:
        data = archive.extractfile(member).read()
        if member.name in rows:
            row = rows[member.name]
            assert len(data) == row["bytes"] and hashlib.sha256(data).hexdigest() == row["sha256"]
        target = root / member.name
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("xb") as file:
            file.write(data)
        target.chmod(member.mode)

original = (root / "input/native-records-v1.tar.gz").read_bytes()
assert hashlib.sha256(original).hexdigest() == "7b14ede379a4679e4bb3269c40928dda6139358b3fb6342fc4767a4c6404ad46"
with tarfile.open(fileobj=io.BytesIO(original)) as archive:
    members = archive.getmembers()
    assert len(members) == len({member.name for member in members}) == 390
    for member in members:
        safe(member)
    rows = json.loads(archive.extractfile("manifest.json").read())
    indexed = {row["path"]: row for row in rows}
    assert len(indexed) == len(rows) == 389
    assert set(indexed) == {member.name for member in members} - {"manifest.json"}
    for member in members:
        data = archive.extractfile(member).read()
        if member.name in indexed:
            row = indexed[member.name]
            assert len(data) == row["bytes"] and hashlib.sha256(data).hexdigest() == row["sha256"]
        target = (root / "input/data" / member.name.removeprefix("corpus/")) if member.name.startswith("corpus/") else (root / "original-corpus-records" / member.name)
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("xb") as file:
            file.write(data)
verifier = root / "source/docs/research/measurements/managed-audit-wire-adversarial/verify-corpus.py"
subprocess.run(["python3", str(verifier), str(root / "input"), str(root / "corpus-verified.json")],
               check=True, timeout=min(30, deadline - time.time()))
assert time.time() < deadline
print(json.dumps({"status": "NewScopeAllSourceAndCorpusVerified_NoCase", "root": str(root),
                  "budget": budget, "source_payloads": len(manifest["files"])}), flush=True)
subprocess.run(["lockf", "-t", "0", "/tmp/g121-mac.lock", "python3", str(root / "build-native.py"), str(root), str(deadline)],
               check=True, timeout=max(0.001, deadline - time.time()))
