#!/usr/bin/env python3.12
"""Offline immutable archive/envelope/accounting checks; never starts an actor."""
import hashlib
import io
import json
import math
from pathlib import Path, PurePosixPath
import re
import struct
import tarfile


def sha(data):
    return hashlib.sha256(data).hexdigest()


def package(data, manifest_name, outside):
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        assert len(names) == len(set(names))
        assert all(member.isfile() and str(PurePosixPath(member.name)) == member.name
                   and not PurePosixPath(member.name).is_absolute()
                   and ".." not in PurePosixPath(member.name).parts and "\\" not in member.name
                   for member in members)
        assert archive.extractfile(manifest_name).read() == outside
        index = json.loads(outside)
        rows = {row["path"]: row for row in index["files"]}
        assert len(rows) == len(index["files"]) and set(rows) == set(names) - {manifest_name}
        files = {}
        for name, row in rows.items():
            raw = archive.extractfile(name).read()
            assert len(raw) == row["bytes"] and sha(raw) == row["sha256"], name
            files[name] = raw
        return index, files


here = Path(__file__).parent
source_path = here.parent / "native-v1/package-v1"
source = (source_path / "source-package.tar.gz").read_bytes()
source_outside = (source_path / "source-package-manifest.json").read_bytes()
assert sha(source) == "ba43fd22a2036de11268aa4ae49e2b9ce748b5bee7bbd3fcb615a86237b86b63"
assert sha(source_outside) == "9a9f9e0c71607179c5d770518cc8f9c21ae55e5a1033090a4452af38bf66aad7"
source_index, source_files = package(source, "source-package-manifest.json", source_outside)
assert len(source_files) == 176
revision = "342bcef5855084a58a7257654d88e9917cef016e"
leaf_sha = "65c73f963edef4ffb81754cf2eea2bd4ecd6a44d7f14178994725b211d0a4c65"
assert source_index["source_revision"] == revision
assert sha(source_files["source/docs/research/measurements/managed-audit-wire-adversarial/native.rs"]) == leaf_sha
receipt = json.loads((here / "raw-pack-receipt-v2.json").read_text())
raw = (here / "raw-runtime-v2.tar.gz").read_bytes()
outside = (here / "raw-manifest-v2.json").read_bytes()
assert len(raw) == receipt["bytes"] and sha(raw) == receipt["sha256"]
assert sha(outside) == receipt["manifest_sha256"]
index, files = package(raw, "manifest.json", outside)
assert len(files) == receipt["payloads"] == 47 and receipt["total_members"] == 48
assert index["source_revision"] == revision and index["native_case_attempts"] == 1
assert index["native_case_retries"] == 0
j = lambda name: json.loads(files[name])
budget = j("budget.json")
assert budget["deadline_epoch"] == budget["start_epoch"] + 600
assert budget["work_seconds"] == 30 and budget["cleanup_seconds"] == 15 and budget["parent_seconds"] == 45
assert budget["cpus"] == 2 and budget["memory_bytes"] == 2 * 1024**3 and budget["disk_bytes"] == 4 * 1024**3
assert budget["policy_revision"] == "749c3ca0602c43dac74976e8db713102f5b15874"
ready = j("controller/ready-v2.json")
assert ready["selected_count"] == 1 and ready["native_attempts"] == 0 and ready["no_cargo_or_rebuild"]
assert ready["sha256"] == "ce4c34d7f480c8b02073d15e0f4ba5b2a11ebfe5b2ceb8bfe2fdc996cc2a8871"
assert ready["source_package_sha256"] == sha(source) and ready["leaf_sha256"] == leaf_sha
assert j("controller/list.result.json")["exit_code"] == 0
assert [line for line in files["controller/list.stdout"].decode().splitlines() if line.endswith(": test")] == [ready["filter"] + ": test"]
lock = j("controller/lock.command.json")
assert lock["wait_allowance_seconds"] == math.floor(budget["deadline_epoch"] - lock["at_epoch"] - 45)
assert lock["argv"][:4] == ["lockf", "-t", str(lock["wait_allowance_seconds"]), "/tmp/g121-mac.lock"]
pre = j("controller/preactor-after-lock.json")
actor = j("controller/actor-start.json")
assert pre["ready"] == ready and pre["lock_command"] == lock and pre["runtime_attempts_so_far"] == 0
assert lock["at_epoch"] < pre["at_epoch"] < actor["epoch"]
assert actor["epoch"] + 45 < budget["deadline_epoch"]
assert pre["free_bytes"] >= budget["floor_bytes"] + budget["disk_bytes"]
assert actor["argv"] == ready["argv"] and actor["runtime_attempt"] == 1
case = j("controller/case.result.json")
assert case["exit_code"] == 0 and not case["timeout"] and case["elapsed_s"] < 45
assert case["runtime_attempts"] == 1 and case["runtime_retries"] == 0
assert j("controller/lock.result.json")["exit_code"] == 0 and not j("controller/lock.result.json")["timeout"]
assert "1 passed; 0 failed; 0 ignored" in files["controller/case.stdout"].decode()
assert files["controller/case.stderr"] == b""
before = j("frozen-inputs/original-input-identities.json")
after = j("original-inputs-after.json")
assert len(before) == len(after) == 6
assert all(after[name]["original_and_copy_unchanged"] and after[name]["sha256"] == row["sha256"]
           and after[name]["bytes"] == row["bytes"] for name, row in before.items())

# Envelope checks supplement the actual Rust ABI decoder; no audit encoder or
# live protocol implementation is reproduced here.
opcodes = []
for unique in range(1, 9):
    request = files[f"native-records-v2/{unique:02}.request.bin"]
    reply = files[f"native-records-v2/{unique:02}.reply.bin"]
    assert len(request) >= 40 and len(reply) >= 16
    assert struct.unpack_from("<I", request)[0] == len(request)
    assert struct.unpack_from("<Q", request, 8)[0] == unique
    assert struct.unpack_from("<IiQ", reply) == (len(reply), 0, unique)
    opcodes.append(struct.unpack_from("<I", request, 4)[0])
assert sum(name.endswith(".request.bin") for name in files) == 8
assert sum(name.endswith(".reply.bin") for name in files) == 8
proto = source_files["source/crates/vmm/src/devices/virtio/fs/proto.rs"].decode()
for name in ["WRITE", "RELEASE", "DESTROY"]:
    definition = re.search(r"pub const " + name + r": u32 = ([0-9]+);", proto)
    assert definition and int(definition.group(1)) not in opcodes
snapshot = lambda stage, member: files[f"native-records-v2/{stage}.member-index-{member}.bin"]
retained = [snapshot("reserved", member) for member in range(2)]
assert all(snapshot("disconnect-after-answer", member) == retained[member] for member in range(2))
assert snapshot("constructed", 1) == snapshot("identified", 1) == retained[1]
corpus = j("corpus-verified.json")
with tarfile.open(fileobj=io.BytesIO(source_files["input/native-records-v1.tar.gz"])) as archive:
    inputs = json.loads(archive.extractfile("corpus/inputs.json").read())
derived_b = len(corpus["selected"]) * inputs["journal_header_bytes"] + inputs["slot_overhead"] + corpus["selected"][0]["intent_max"] + corpus["selected"][0]["terminal_max"]
assert sum(map(len, retained)) == derived_b == corpus["aggregate_B"]
events = files["native-records-v2/events.log"].decode()
facts = ["request=8 actual_audit_reply=Reply { stage: Reserved, cause: None, call: 0,",
         "sequence_result=Ok(()) attempted_requests=8",
         "owned_end=true client_EBADF=true answer_join=Ok(Ok(())) server_EBADF=true",
         "disconnect_after_answer_readiness=Ready { remaining: 0, objects: 2 }",
         "disconnect_individual_lease_stage_and_live_cursors=Unobserved",
         "server_dropped=true root_fd_EBADF=true", "actual_root_absent_after_owner_drop=true"]
assert all(fact in events for fact in facts)
assert events.index(facts[2]) < events.index(facts[3]) < events.index(facts[5]) < events.index(facts[6])
assert "actual_audit_reply=Reply { stage: Closed" not in events
observations = re.findall(r"snapshot=(\S+) member_index=([01]) dev=(\d+) ino=(\d+) nlink=(\d+) length=(\d+)", events)
assert len(observations) == 8
identities = []
for member in range(2):
    rows = [row for row in observations if int(row[1]) == member]
    assert {row[0] for row in rows} == {"constructed", "identified", "reserved", "disconnect-after-answer"}
    assert len({(row[2], row[3]) for row in rows}) == 1 and all(row[4] == "1" for row in rows)
    identities.append((rows[0][2], rows[0][3]))
assert identities[0] != identities[1]
final = re.findall(r"held_object_final path=.* dev=(\d+) ino=(\d+) nlink=1", events)
assert sorted(final) == sorted(identities)
witnesses = re.findall(r"^witness_fd=(\d+) EBADF=true$", events, re.MULTILINE)
assert len(witnesses) == len(set(witnesses)) == 2
print(json.dumps({"status": "VerifiedOneActualNativePass", "source_revision": revision,
                  "source_payloads": len(source_files), "raw_payloads": len(files), "total_raw_members": len(files) + 1,
                  "runtime_attempts": 1, "runtime_retries": 0, "actual_frame_pairs": 8,
                  "aggregate_B": derived_b, "retained_lengths": list(map(len, retained)),
                  "individual_postdisconnect_lease_and_cursors": "Unobserved", "original_named_inputs_unchanged": len(before),
                  "executable_sha256": ready["sha256"], "archive_sha256": sha(raw), "manifest_sha256": sha(outside),
                  "limits": "No main/integration/VM/physical-quota/durability/fairness/semantic-authority/cost closure"}, indent=2))
