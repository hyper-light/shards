#!/usr/bin/env python3
"""Verify the immutable first native record and source packages; starts no actors."""
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import struct
import sys
import tarfile
import tomllib


def digest(data):
    return hashlib.sha256(data).hexdigest()


def package(raw, manifest_name, outside):
    with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        assert len(names) == len(set(names))
        assert all(member.isfile() and str(PurePosixPath(member.name)) == member.name
                   and not PurePosixPath(member.name).is_absolute()
                   and ".." not in PurePosixPath(member.name).parts
                   and "\\" not in member.name for member in members)
        assert archive.extractfile(manifest_name).read() == outside
        manifest = json.loads(outside)
        rows = {row["path"]: row for row in manifest["files"]}
        assert len(rows) == len(manifest["files"])
        assert set(rows) == set(names) - {manifest_name}
        payloads = {}
        for name, row in rows.items():
            data = archive.extractfile(name).read()
            assert len(data) == row["bytes"] and digest(data) == row["sha256"], name
            payloads[name] = data
        return manifest, payloads


root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent / "native-v2"
raw = (root / "raw-native-v1.tar.gz").read_bytes()
index = (root / "raw-manifest-v1.json").read_bytes()
assert digest(raw) == "fed4897abc3072942c88e2bc07b158ea3b07ce9eb7640f014a1b4445e57403f0"
assert digest(index) == "92e39f67d524766e2e0f8d42ebd734a4261d9adebffbeb140a2ee810791edef0"
manifest, files = package(raw, "manifest.json", index)
assert len(files) == 114 and manifest["native_case_attempts"] == 1
assert manifest["native_case_retries"] == 0
source = (root / "source-package.tar.gz").read_bytes()
source_index = (root / "source-package-manifest.json").read_bytes()
assert digest(source) == "c11862f9d9b8c40dbab9125380a4b61aab9dcc15ea6cf71e20207808348c9c31"
assert digest(source_index) == "22fa60d58e9623581333b0732fe4a35c0250c2d735e98fb6670175826c4c1ff0"
source_manifest, source_files = package(source, "source-package-manifest.json", source_index)
assert len(source_files) == 176
assert source_manifest["source_revision"] == manifest["source_revision"]
assert digest(source_files["source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"]) == "75427cca006a347f7b6a02ef88203e3b947db3d5f74157192001d1dc097e8a00"

# Cargo prunes absent workspace packages in this private dependency closure. Every
# retained external package must still have its original source/version/checksum.
original_lock = tomllib.loads(source_files["source/Cargo.lock"].decode())
actual_lock = tomllib.loads(files["build-records/actual-Cargo.lock"].decode())
identity = lambda row: (row["name"], row["version"], row.get("source"), row.get("checksum"))
original_packages = {identity(row) for row in original_lock["package"]}
actual_external = {identity(row) for row in actual_lock["package"] if "source" in row}
assert actual_external <= original_packages

result = json.loads(files["case-controller-v1/case.result.json"])
assert result["exit_code"] == 0 and not result["timeout"]
assert result["native_case_attempts"] == 1 and result["native_retries"] == 0
for stage in ["toolchain", "policy", "build", "list", "clippy"]:
    actual = json.loads(files[f"build-records/{stage}.result.json"])
    assert actual["exit_code"] == 0 and not actual["timeout"]
ready = json.loads(files["build-records/ready.json"])
assert ready["selected_count"] == 1 and ready["native_cases_run"] == 0
assert ready["sha256"] == "a502bd28e1e55f27ff204788c3737fe148c5ceb5441e6f9287f56e15ba8cea30"
assert "1 passed; 0 failed; 0 ignored" in files["case-controller-v1/case.stdout"].decode()

# Offline frame accounting checks the retained envelope, never substitutes for
# the Rust shared parser's typed result or the actual held-file assertions.
for unique in range(1, 25):
    request = files[f"native-records-v1/{unique:02}.request.bin"]
    reply = files[f"native-records-v1/{unique:02}.reply.bin"]
    assert len(request) >= 40 and len(reply) >= 16
    assert struct.unpack_from("<I", request)[0] == len(request)
    assert struct.unpack_from("<Q", request, 8)[0] == unique
    assert struct.unpack_from("<IiQ", reply) == (len(reply), 0, unique)

stages = ["constructed", "identified", "reserved", "duplicate-reserve",
          "intent-written", "intent-committed", "duplicate-intent",
          "terminal-written", "terminal-committed", "duplicate-terminal",
          "closed", "duplicate-close", "closed-inspect", "neighbor-refused", "released"]
snapshot = lambda stage, member: files[f"native-records-v1/{stage}.member-index-{member}.bin"]
for before, after in [("reserved", "duplicate-reserve"), ("intent-committed", "duplicate-intent"),
                      ("terminal-committed", "duplicate-terminal"), ("closed", "duplicate-close")]:
    assert all(snapshot(before, member) == snapshot(after, member) for member in range(2))
neighbor = snapshot("constructed", 1)
assert all(snapshot(stage, 1) == neighbor for stage in stages)
assert snapshot("terminal-committed", 0) == snapshot("released", 0)
verified_corpus = json.loads(files["corpus-verified.json"])
budget = verified_corpus["aggregate_B"]
assert sum(len(snapshot("released", member)) for member in range(2)) == budget
events = files["native-records-v1/events.log"].decode()
for fact in ["sequence_result=Ok(()) attempted_requests=24", "stage: Empty, cause: Exhausted",
             "owned_end=true client_EBADF=true answer_join=Ok(Ok(())) server_EBADF=true",
             "witness_fd=7 EBADF=true", "witness_fd=8 EBADF=true",
             "server_dropped=true root_fd_EBADF=true", "actual_root_absent_after_owner_drop=true"]:
    assert fact in events, fact
summary = {"status": "Verified", "raw_payloads": len(files), "raw_total_members": len(files) + 1,
           "source_payloads": len(source_files), "external_package_identities_unchanged": len(actual_external),
           "native_case_attempts": 1, "runtime_retries": 0, "actual_request_reply_pairs": 24,
           "aggregate_B": budget, "retained_lengths": [len(snapshot("released", member)) for member in range(2)],
           "archive_sha256": digest(raw), "manifest_sha256": digest(index),
           "limits": "No physical quota, durability, fairness, live semantic effect, population/kernel or management closure proof"}
print(json.dumps(summary, indent=2))
