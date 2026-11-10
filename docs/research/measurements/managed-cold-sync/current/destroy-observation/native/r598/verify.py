#!/usr/bin/env python3
"""Verify original raw receipts and reviewed source; execute no Rust or actor."""
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
    archive = base64.b64decode((here / "raw.tar.gz.b64").read_text().strip(), validate=True)
    assert len(archive) == 11113
    assert sha(archive) == "d8070a3ca76835cbc82066bc1c98855ad85e954e72a26209823485e49563258d"
    members = {}
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        for entry in tar.getmembers():
            name = PurePosixPath(entry.name)
            assert entry.isfile() and not name.is_absolute() and ".." not in name.parts
            assert entry.name not in members
            members[entry.name] = tar.extractfile(entry).read()
    manifest = json.loads(members.pop("manifest.json"))
    assert set(members) == set(manifest["files"])
    for name, body in members.items():
        record = manifest["files"][name]
        assert len(body) == record["bytes"] and sha(body) == record["sha256"], name
    packed = base64.b64decode((here.parent / "inputs-v1/input.json.gz.b64").read_text().strip(), validate=True)
    assert sha(packed) == "fa8e3dfa0f2a5463029f5ead19b79d3e8be96ddb0d039440d8c7e2732a8a6bd3"
    bundle = json.loads(gzip.decompress(packed))
    snapshot = json.loads(members["inputs/source-snapshot.json"])
    assert set(bundle["version"]["files"]) == set(snapshot["files"])
    for name, record in bundle["version"]["files"].items():
        body = record["text"].encode()
        assert len(body) == snapshot["files"][name]["bytes"]
        assert sha(body) == snapshot["files"][name]["sha256"], name
    row = json.loads(members["raw/summary.json"])
    assert [(item["stage"], item["exit"]) for item in row["commands"]] == [("strict-production", 0), ("compile", 101)]
    assert row["case"] is None and row["native_case_attempts"] == 0
    assert row["controller_fds_before"] == row["controller_fds_after"] == 4
    assert not row["expected_ordinary_result"]
    diagnostics = json.loads(members["raw/compiler-diagnostics.json"])
    errors = [item["message"] for item in diagnostics if item["message"]["level"] == "error"]
    assert len(errors) == 1 and errors[0]["code"]["code"] == "E0277"
    assert errors[0]["message"] == "`SharedWitness` doesn't implement `Debug`"
    name = "docs/research/measurements/managed-cold-sync/current/destroy-observation/observer.rs"
    old = bundle["version"]["files"][name]["text"].encode()
    corrected = (here.parents[1] / "observer.rs").read_bytes()
    assert old.count(b"#[derive(Default)]") == 1
    assert corrected == old.replace(b"#[derive(Default)]", b"#[derive(Default, Debug)]", 1)
    result = {"run": "r598", "archive_sha256": sha(archive), "raw_payloads_verified": len(members),
              "raw_members_including_manifest": len(members) + 1,
              "source_snapshot_files_verified": len(snapshot["files"]),
              "source_revision": bundle["source_revision"],
              "production_strict_lint": 0, "lib_test_compile": 101,
              "native_case_attempts": 0, "native_pass_fail_ignore_counts": None,
              "corrected_observer_sha256": sha(corrected), "correction_rust_compile_lint_native": "UNRUN",
              "full_archive": json.loads(members["r598-archive.json"]),
              "full_archive_compressed_bytes_locally_verified": False,
              "full_archive_partial_prefix_only": (here / "full-archive-first-part.b64").stat().st_size,
              "no_source_or_raw_deletion": True}
    if args.out:
        with args.out.open("x") as output:
            json.dump(result, output, indent=2)
            output.write("\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
