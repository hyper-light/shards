#!/usr/bin/env python3.12
"""Offline immutable package/frame/file accounting; never starts native actors."""
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import struct
import sys
import tarfile
import tomllib


def digest(data):
    return hashlib.sha256(data).hexdigest()


def package(data, index_name, outside):
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        assert len(names) == len(set(names))
        assert all(member.isfile() and str(PurePosixPath(member.name)) == member.name
                   and not PurePosixPath(member.name).is_absolute()
                   and ".." not in PurePosixPath(member.name).parts and "\\" not in member.name
                   for member in members)
        assert archive.extractfile(index_name).read() == outside
        index = json.loads(outside)
        rows = {row["path"]: row for row in index["files"]}
        assert len(rows) == len(index["files"])
        assert set(rows) == set(names) - {index_name}
        files = {}
        for name, row in rows.items():
            raw = archive.extractfile(name).read()
            assert len(raw) == row["bytes"] and digest(raw) == row["sha256"], name
            files[name] = raw
        return index, files


root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent
receipt = json.loads((root / "raw-pack-receipt.json").read_text())
raw = (root / "raw-native-v1.tar.gz").read_bytes()
outside = (root / "raw-manifest-v1.json").read_bytes()
assert digest(raw) == receipt["sha256"] and len(raw) == receipt["bytes"]
assert digest(outside) == receipt["manifest_sha256"]
index, files = package(raw, "manifest.json", outside)
assert len(files) == receipt["payloads"] and len(files) + 1 == receipt["total_members"]
assert index["native_case_attempts"] == 1 and index["native_case_retries"] == 0
source = (root / "package-v1/source-package.tar.gz").read_bytes()
source_outside = (root / "package-v1/source-package-manifest.json").read_bytes()
assert digest(source) == "37448e07bd227d19346d990d188bfe46ed1f0a43c1b789666262c82d7003970f"
assert digest(source_outside) == "82872430c5e0d1e2fd9a2c328b220d6a66dcd0473b7a170f9a564ccf50730fca"
source_index, source_files = package(source, "source-package-manifest.json", source_outside)
assert len(source_files) == 176
assert source_index["source_revision"] == index["source_revision"] == "82e7243104aaf15b70a373d49fcdf4293136e012"
assert digest(source_files["source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"]) == "228bb9e67e1e699d969a71de817c5a9c6e461ee18e07d98aa070029b07b59e4e"
identity = lambda row: (row["name"], row["version"], row.get("source"), row.get("checksum"))
original_packages = {identity(row) for row in tomllib.loads(source_files["source/Cargo.lock"].decode())["package"]}
actual_external = {identity(row) for row in tomllib.loads(files["build-records/actual-Cargo.lock"].decode())["package"] if "source" in row}
assert actual_external <= original_packages

case = json.loads(files["case-controller-v1/case.result.json"])
assert case["exit_code"] == 0 and not case["timeout"]
assert case["native_case_attempts"] == 1 and case["native_retries"] == 0
for stage in ["toolchain", "policy", "build", "list", "clippy"]:
    result = json.loads(files[f"build-records/{stage}.result.json"])
    assert result["exit_code"] == 0 and not result["timeout"]
ready = json.loads(files["build-records/ready.json"])
assert ready["selected_count"] == 1 and ready["native_cases_run"] == 0
assert ready["filter"] == "managed_audit_wire_adversarial::reserved_close_retains_extent_and_refuses_reopening"
dispatch = json.loads(files["case-controller-v1/dispatch-before-case.json"])
assert dispatch["ready"] == ready and dispatch["case_native_count"] == 0
assert dispatch["at_epoch"] < json.loads(files["case-controller-v1/actor-start.json"])["epoch"]
assert "1 passed; 0 failed; 0 ignored" in files["case-controller-v1/case.stdout"].decode()

# This accounts for the raw FUSE envelopes, never implements the typed audit
# codec or substitutes for the actual Rust parser/held-object path.
opcodes = []
for unique in range(1, 20):
    request = files[f"native-records-v1/{unique:02}.request.bin"]
    reply = files[f"native-records-v1/{unique:02}.reply.bin"]
    assert len(request) >= 40 and len(reply) >= 16
    assert struct.unpack_from("<I", request)[0] == len(request)
    assert struct.unpack_from("<Q", request, 8)[0] == unique
    assert struct.unpack_from("<IiQ", reply) == (len(reply), 0, unique)
    opcodes.append(struct.unpack_from("<I", request, 4)[0])
assert len([name for name in files if name.endswith(".request.bin")]) == 19
assert len([name for name in files if name.endswith(".reply.bin")]) == 19
proto = source_files["source/crates/vmm/src/devices/virtio/fs/proto.rs"].decode()
write_opcode = re.search(r"pub const WRITE: u32 = ([0-9]+);", proto)
assert write_opcode and int(write_opcode.group(1)) not in opcodes

snapshot = lambda stage, member: files[f"native-records-v1/{stage}.member-index-{member}.bin"]
stages = ["reserved-close", "reserved-close-duplicate", "reserved-close-inspect",
          "closed-reserve-refused", "closed-intent-refused", "closed-terminal-refused",
          "closed-final-inspect", "closed-neighbor-refused", "closed-neighbor-identified",
          "closed-reserved-released"]
retained = [snapshot("reserved", member) for member in range(2)]
assert all(snapshot(stage, member) == retained[member] for stage in stages for member in range(2))
assert snapshot("constructed", 1) == snapshot("identified", 1) == retained[1]
corpus = json.loads(files["corpus-verified.json"])
assert sum(map(len, retained)) == corpus["aggregate_B"]
events = files["native-records-v1/events.log"].decode()
for unique in range(9, 16):
    cause = "Closed" if unique in [12, 13, 14] else "None"
    assert f"request={unique} actual_audit_reply=Reply {{ stage: Closed, cause: {cause}," in events
assert "request=16 actual_audit_reply=Reply { stage: Empty, cause: Exhausted," in events
assert "request=17 actual_audit_reply=Reply { stage: Empty, cause: None," in events
for fact in ["sequence_result=Ok(()) attempted_requests=19",
             "owned_end=true client_EBADF=true answer_join=Ok(Ok(())) server_EBADF=true",
             "server_dropped=true root_fd_EBADF=true", "actual_root_absent_after_owner_drop=true"]:
    assert fact in events, fact
witnesses = re.findall(r"^witness_fd=([0-9]+) EBADF=true$", events, re.MULTILINE)
assert len(witnesses) == 2 and len(set(witnesses)) == 2
print(json.dumps({"status": "Verified", "source_revision": source_index["source_revision"],
                  "raw_payloads": len(files), "raw_total_members": len(files) + 1,
                  "source_payloads": len(source_files), "external_package_identities_unchanged": len(actual_external),
                  "native_case_attempts": 1, "runtime_retries": 0, "actual_frame_pairs": 19,
                  "fuse_WRITE_frames": 0, "aggregate_B": corpus["aggregate_B"],
                  "retained_lengths": list(map(len, retained)), "witness_fds": witnesses,
                  "archive_sha256": digest(raw), "manifest_sha256": digest(outside),
                  "limits": "No in-flight/fairness/physical-quota/durability/semantic-effect/parent-kernel/VM/management closure proof"}, indent=2))
