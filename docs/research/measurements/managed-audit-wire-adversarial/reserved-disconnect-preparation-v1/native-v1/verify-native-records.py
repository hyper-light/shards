#!/usr/bin/env python3.12
"""Verify immutable files offline; this script never starts a native actor."""
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
        assert len(rows) == len(index["files"]) and set(rows) == set(names) - {index_name}
        files = {}
        for name, row in rows.items():
            raw = archive.extractfile(name).read()
            assert len(raw) == row["bytes"] and digest(raw) == row["sha256"], name
            files[name] = raw
        return index, files


root = Path(__file__).parent
source = (root / "package-v1/source-package.tar.gz").read_bytes()
source_outside = (root / "package-v1/source-package-manifest.json").read_bytes()
assert digest(source) == "ba43fd22a2036de11268aa4ae49e2b9ce748b5bee7bbd3fcb615a86237b86b63"
assert digest(source_outside) == "9a9f9e0c71607179c5d770518cc8f9c21ae55e5a1033090a4452af38bf66aad7"
source_index, source_files = package(source, "source-package-manifest.json", source_outside)
assert len(source_files) == 176
revision = "342bcef5855084a58a7257654d88e9917cef016e"
assert source_index["source_revision"] == revision
assert digest(source_files["source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"]) == "65c73f963edef4ffb81754cf2eea2bd4ecd6a44d7f14178994725b211d0a4c65"
if "--source-only" in sys.argv:
    print(json.dumps({"status": "SourcePackageVerified_NoBuildOrActorByVerifier",
                      "source_revision": revision, "source_payloads": len(source_files),
                      "package_sha256": digest(source), "native_case_attempts": 0}, indent=2))
    raise SystemExit(0)

receipt = json.loads((root / "raw-pack-receipt.json").read_text())
raw = (root / "raw-native-v1.tar.gz").read_bytes()
outside = (root / "raw-manifest-v1.json").read_bytes()
assert digest(raw) == receipt["sha256"] and len(raw) == receipt["bytes"]
assert digest(outside) == receipt["manifest_sha256"]
index, files = package(raw, "manifest.json", outside)
assert len(files) == receipt["payloads"] and len(files) + 1 == receipt["total_members"]
assert index["native_case_retries"] == 0
assert index["source_revision"] == revision
identity = lambda row: (row["name"], row["version"], row.get("source"), row.get("checksum"))
original_packages = {identity(row) for row in tomllib.loads(source_files["source/Cargo.lock"].decode())["package"]}
actual_external = {identity(row) for row in tomllib.loads(files["build-records/actual-Cargo.lock"].decode())["package"] if "source" in row}
assert actual_external <= original_packages
for stage in ["toolchain", "policy", "build", "list", "clippy"]:
    result = json.loads(files[f"build-records/{stage}.result.json"])
    assert result["exit_code"] == 0 and not result["timeout"]
ready = json.loads(files["build-records/ready.json"])
assert ready["selected_count"] == 1 and ready["native_cases_run"] == 0
assert ready["filter"] == "managed_audit_wire_adversarial::reserved_client_disconnect_retains_owned_extent"
dispatch = json.loads(files["case-controller-v1/dispatch-before-case.json"])
assert dispatch["ready"] == ready and dispatch["case_native_count"] == 0
if "--unrun" in sys.argv:
    assert index["native_case_attempts"] == 0
    assert not any(name.startswith("native-records-v1/") for name in files)
    assert "case-controller-v1/actor-start.json" not in files
    assert "case-controller-v1/case.stdout" not in files
    assert "case-controller-v1/case.result.json" not in files
    outcome = json.loads(files["case-controller-v1/outcome.json"])
    assert outcome["status"] == "CoverageMiss_PreactorLockRefusal_NoNativeActor"
    assert outcome["runtime_attempts"] == 0 and outcome["no_retry"]
    assert outcome["actor_start_absent"] and outcome["native_records_absent"] and outcome["native_tmp_empty"]
    refusal = json.loads(files["case-controller-v1/first-native-lock-refusal.json"])
    actual = refusal["tool_response"]["structuredContent"]["result"]
    assert actual["exit_code"] == 75 and actual["output"] == "lockf: /tmp/g121-mac.lock: already locked\n"
    assert actual["runner"] == index["runner"] == "e30189ea29182644fe5a4b2e06c7dac2"
    assert dispatch["at_epoch"] + 45 < outcome["deadline_epoch"]
    assert outcome["at_epoch"] + 45 > outcome["deadline_epoch"]
    artifact = json.loads(files["build-records/artifact.json"])
    assert artifact["sha256"] == ready["sha256"] == "ce4c34d7f480c8b02073d15e0f4ba5b2a11ebfe5b2ceb8bfe2fdc996cc2a8871"
    assert dispatch["package_sha256"] == digest(source)
    assert dispatch["leaf_sha256"] == digest(source_files["source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"])
    assert dispatch["lib_include_sha256"] == digest(source_files["source/crates/vmm/src/lib.rs"])
    assert [line for line in files["build-records/list.stdout"].decode().splitlines() if line.endswith(": test")] == [ready["filter"] + ": test"]
    print(json.dumps({"status": "VerifiedActualBuildAndCoverageMiss_NoNativePass",
                      "source_revision": revision, "source_payloads": len(source_files),
                      "raw_payloads": len(files), "raw_total_members": len(files) + 1,
                      "actual_native_build_exit": 0, "strict_clippy_exit": 0, "selected_count": 1,
                      "external_package_identities_unchanged": len(actual_external),
                      "executable_sha256": ready["sha256"], "preactor_lock_exit": 75,
                      "runtime_attempts": 0, "runtime_retries": 0, "actual_frame_pairs": 0,
                      "runtime_and_native_cleanup": "Unrun", "archive_sha256": digest(raw),
                      "manifest_sha256": digest(outside)}, indent=2))
    raise SystemExit(0)
assert index["native_case_attempts"] == 1
case = json.loads(files["case-controller-v1/case.result.json"])
assert case["exit_code"] == 0 and not case["timeout"]
assert case["native_case_attempts"] == 1 and case["native_retries"] == 0
actor = json.loads(files["case-controller-v1/actor-start.json"])
assert dispatch["at_epoch"] < actor["epoch"] < actor["deadline_epoch"]
assert "1 passed; 0 failed; 0 ignored" in files["case-controller-v1/case.stdout"].decode()

# Envelope accounting only; the real Rust case checked audit types/codecs.
opcodes = []
for unique in range(1, 9):
    request = files[f"native-records-v1/{unique:02}.request.bin"]
    reply = files[f"native-records-v1/{unique:02}.reply.bin"]
    assert len(request) >= 40 and len(reply) >= 16
    assert struct.unpack_from("<I", request)[0] == len(request)
    assert struct.unpack_from("<Q", request, 8)[0] == unique
    assert struct.unpack_from("<IiQ", reply) == (len(reply), 0, unique)
    opcodes.append(struct.unpack_from("<I", request, 4)[0])
assert len([name for name in files if name.endswith(".request.bin")]) == 8
assert len([name for name in files if name.endswith(".reply.bin")]) == 8
proto = source_files["source/crates/vmm/src/devices/virtio/fs/proto.rs"].decode()
for name in ["WRITE", "RELEASE", "DESTROY"]:
    definition = re.search(r"pub const " + name + r": u32 = ([0-9]+);", proto)
    assert definition and int(definition.group(1)) not in opcodes
snapshot = lambda stage, member: files[f"native-records-v1/{stage}.member-index-{member}.bin"]
retained = [snapshot("reserved", member) for member in range(2)]
assert all(snapshot("disconnect-after-answer", member) == retained[member] for member in range(2))
assert snapshot("constructed", 1) == snapshot("identified", 1) == retained[1]
corpus = json.loads(files["corpus-verified.json"])
assert sum(map(len, retained)) == corpus["aggregate_B"]
events = files["native-records-v1/events.log"].decode()
facts = ["request=8 actual_audit_reply=Reply { stage: Reserved, cause: None, call: 0,",
         "sequence_result=Ok(()) attempted_requests=8",
         "owned_end=true client_EBADF=true answer_join=Ok(Ok(())) server_EBADF=true",
         "disconnect_after_answer_readiness=Ready { remaining: 0, objects: 2 }",
         "disconnect_individual_lease_stage_and_live_cursors=Unobserved",
         "server_dropped=true root_fd_EBADF=true", "actual_root_absent_after_owner_drop=true"]
for fact in facts:
    assert fact in events, fact
assert events.index(facts[2]) < events.index(facts[3]) < events.index(facts[5]) < events.index(facts[6])
assert "actual_audit_reply=Reply { stage: Closed" not in events
witnesses = re.findall(r"^witness_fd=([0-9]+) EBADF=true$", events, re.MULTILINE)
assert len(witnesses) == len(set(witnesses)) == 2
print(json.dumps({"status": "Verified", "source_revision": revision,
                  "raw_payloads": len(files), "raw_total_members": len(files) + 1,
                  "source_payloads": len(source_files), "external_package_identities_unchanged": len(actual_external),
                  "native_case_attempts": 1, "runtime_retries": 0, "actual_frame_pairs": 8,
                  "fuse_WRITE_RELEASE_DESTROY_frames": 0, "aggregate_B": corpus["aggregate_B"],
                  "retained_lengths": list(map(len, retained)), "witness_fds": witnesses,
                  "individual_post_disconnect_lease_and_cursors": "Unobserved",
                  "archive_sha256": digest(raw), "manifest_sha256": digest(outside),
                  "limits": "No main/integration/VM/admission/fairness/physical-quota/durability/semantic-effect/cost closure proof"}, indent=2))
