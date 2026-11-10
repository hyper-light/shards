#!/usr/bin/env python3
"""Verify the byte-preserving Windows receipts and frozen crate identities."""
import base64
import gzip
import hashlib
import json
from pathlib import Path, PurePosixPath
import subprocess


def sha(body):
    return hashlib.sha256(body).hexdigest()


def main():
    here = Path(__file__).resolve().parent
    receipt = json.loads((here / "archive.json").read_text())
    archive = base64.b64decode("".join((here / receipt["local_file"]).read_text().split()), validate=True)
    assert len(archive) == receipt["bytes"] and sha(archive) == receipt["sha256"]
    record = json.loads(gzip.decompress(archive))
    assert len(record["payloads"]) == receipt["payloads"]
    files = {}
    for entry in record["payloads"]:
        path = PurePosixPath(entry["path"])
        assert not path.is_absolute() and ".." not in path.parts and entry["path"] not in files
        body = base64.b64decode(entry["data_base64"], validate=True)
        assert len(body) == entry["bytes"] and sha(body) == entry["sha256"]
        files[entry["path"]] = body
    version = json.loads(files["input.json"])["version"]
    assert version["revision"] == receipt["source_revision"]
    source_count = 0
    for name, value in version["files"].items():
        body = value["text"].encode()
        assert sha(body) == value["sha256"]
        if name.startswith("crates/") or name.endswith(".rs"):
            assert body == subprocess.check_output(["git", "show", receipt["source_revision"] + ":" + name])
            source_count += 1
    assert sha(files["driver-v2.ps1"]) == receipt["driver_v2_sha256"]
    summary = json.loads(files["execution-v2/summary.json"])
    for command in summary["commands"]:
        for stream in ["stdout", "stderr"]:
            assert sha(files["execution-v2/raw/" + command["stage"] + "." + stream]) == command[stream + "_sha256"]
        assert command["exit"] == 0 and command["raw_capture_complete"] and not command["timed_out"]
    before = json.loads(files["execution/summary.json"])
    print(json.dumps({"payloads_verified": len(files), "exact_frozen_crate_and_import_files": source_count,
                      "archive_sha256": receipt["sha256"], "all_unique_safe_relative": True,
                      "before_command_exits": [(c["stage"], c["exit"]) for c in before["commands"]],
                      "actual_ordinary": summary["ordinary"],
                      "after_command_exits": [(c["stage"], c["exit"]) for c in summary["commands"]]}))


if __name__ == "__main__":
    main()
