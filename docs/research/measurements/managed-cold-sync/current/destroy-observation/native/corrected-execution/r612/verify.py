#!/usr/bin/env python3
"""Verify the one native result and preserve its failed controller FD gate."""
import argparse
import base64
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tarfile


def sha(body):
    return hashlib.sha256(body).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    here = Path(__file__).resolve().parent
    body = base64.b64decode((here / "raw.tar.gz.b64").read_text().strip(), validate=True)
    assert len(body) == 10456
    assert sha(body) == "09321ea8150b6c8e9ff54063076e509c03202f1f9595a3a0185ff827b374559c"
    files = {}
    with tarfile.open(fileobj=io.BytesIO(body), mode="r:gz") as tar:
        for entry in tar.getmembers():
            path = PurePosixPath(entry.name)
            assert entry.isfile() and not path.is_absolute() and ".." not in path.parts
            assert entry.name not in files
            files[entry.name] = tar.extractfile(entry).read()
    manifest = json.loads(files.pop("manifest.json"))
    assert set(files) == set(manifest["files"])
    for name, data in files.items():
        expected = manifest["files"][name]
        assert len(data) == expected["bytes"] and sha(data) == expected["sha256"], name
    packet = base64.b64decode((here.parents[1] / "inputs-corrected-v1/input.json.gz.b64").read_text().strip(), validate=True)
    assert sha(packet) == "855a2f989975c79d31b7f2b48de4e13013d978dfb1ae21773eab0939753b6976"
    bundle = json.loads(gzip.decompress(packet))
    snapshot = json.loads(files["inputs/source-snapshot.json"])
    assert snapshot["governing_policy_revision"] == "ca3ab10c151aa5539e757f1cc76029746ccbb561"
    assert snapshot["source_revision"] == "10f282bc642f37be54c701bd4ce389574b641e1d"
    assert set(snapshot["files"]) == set(bundle["version"]["files"])
    for name, record in bundle["version"]["files"].items():
        data = record["text"].encode()
        assert len(data) == snapshot["files"][name]["bytes"]
        assert sha(data) == snapshot["files"][name]["sha256"], name
    row = json.loads(files["raw/summary.json"])
    outer = json.loads(files["raw/outer.json"])
    case = row["case"]
    assert [(item["stage"], item["exit"]) for item in row["commands"]] == [
        ("strict-production", 0), ("compile", 0), ("strict-test", 0)]
    assert row["native_case_attempts"] == 1 and case["exit"] == 0 and not case["timed_out"]
    assert row["selection"]["exit"] == 0
    assert case["observer_registered_before_release"]
    assert case["test_exit_observed"] and case["standin_exit_observed"]
    output = files["raw/ordinary/stdout"]
    assert sha(output) == case["stdout_sha256"]
    assert sha(files["raw/ordinary/stderr"]) == case["stderr_sha256"]
    assert b"1 passed; 0 failed; 0 ignored; 0 measured; 46 filtered out;" in output
    events = [line.removeprefix("TEARDOWN_EVENT ") for line in output.decode().splitlines()
              if line.startswith("TEARDOWN_EVENT ")]
    expected = ["SharedDropEntered"]
    for name in ["State", "Changed", "Tail", "Drained"]:
        expected += [f"DestroyEntered({name})", f"DestroyReturned {{ object: {name}, code: 0 }}",
                     f"PointeeDropReturned({name})"]
    expected.append("OriginalSharedFieldsDropped")
    assert events == expected == row["events"]
    for key in ["exact_derived_events_match", "actual_parent_echild_marker", "eight_actual_join_marker",
                "field_and_channel_marker", "actual_one_pass_no_ignore", "vsock_directory_absent"]:
        assert row[key], key
    assert b"CONCURRENT_CONNECTED port=1024 result=Ok(())\n" in output
    assert b"CONCURRENT_CONNECTED port=1025 result=Ok(())\n" in output
    assert b"CONCURRENT_WAIT_RETURNED id=0 result=Ok(Some(Stopped))\n" in output
    assert b"CONCURRENT_WAIT_RETURNED id=1 result=Ok(Some(Stopped))\n" in output
    # Preserve the actual failed outer assertion rather than redefining success.
    assert row["controller_fds_before"] == 4 and row["controller_fds_after"] == 5
    assert not row["controller_fds_returned"] and not row["expected_ordinary_result"]
    assert outer["exit"] == 1
    assert {"kill_owned_group_errno": 1} in case["cleanup"]
    identity = json.loads(files["raw/post-run-identity.json"])
    assert identity["post_run_all_source_hashes_match"]
    assert identity["frozen_binary"]["sha256"] == row["binary"]["sha256"]
    result = {"run": "r612", "governing_policy_revision": snapshot["governing_policy_revision"],
              "source_revision": snapshot["source_revision"], "raw_archive_sha256": sha(body),
              "raw_payloads_verified": len(files), "raw_members_including_manifest": len(files) + 1,
              "source_snapshot_files_verified": len(snapshot["files"]),
              "production_lint": 0, "lib_test_compile": 0, "test_lint": 0,
              "native_case_attempts": 1, "actual_native_passed": 1, "actual_native_failed": 0,
              "actual_native_ignored": 0, "filtered": 46, "actual_events": events,
              "all_eight_join_and_echild_markers": True, "both_outer_note_exit": True,
              "parent_wait_exit": case["exit"], "owned_directory_absent": row["vsock_directory_absent"],
              "overall_gate_exit": outer["exit"], "controller_fds_before": 4, "controller_fds_after": 5,
              "extra_fd_identity_attribution": "UNKNOWN; not observed in the live controller",
              "final_owned_group_signal_errno": 1, "enclosing_arc_deallocation": "UNKNOWN; no hook",
              "native_error_retention": "UNKNOWN; not injected/observed",
              "frozen_binary": identity["frozen_binary"], "full_archive": manifest["full_archive"],
              "full_archive_compressed_bytes_locally_verified": False,
              "native_runtime_repeated": False, "deadline_increased": False}
    if args.out:
        with args.out.open("x") as destination:
            json.dump(result, destination, indent=2)
            destination.write("\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
