#!/usr/bin/env python3
"""Verify frozen raw evidence without extracting files or starting actors."""
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tarfile

root = Path(__file__).parent
raw = (root / "continuation-v1.tar.gz").read_bytes()
assert len(raw) == 190427
assert hashlib.sha256(raw).hexdigest() == "e5e1f5bc3c3a0c60bf1f332f38f5f889608cca3e9bb60dee6d350f8731221aef"
index = json.loads((root / "continuation-v1.member-hashes.json").read_bytes())
summary = json.loads((root / "continuation-v1.validated-cases.json").read_bytes())
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    members = archive.getmembers()
    names = [member.name for member in members]
    assert len(names) == len(set(names)) == 1514
    assert set(names) == set(index) | {"member-hashes.json"}
    assert json.loads(archive.extractfile("member-hashes.json").read()) == index
    payloads = {}
    for member in members:
        path = PurePosixPath(member.name)
        assert member.isfile() and not path.is_absolute() and ".." not in path.parts
        assert "\\" not in member.name and str(path) == member.name
        data = archive.extractfile(member).read()
        assert len(data) == member.size
        if member.name != "member-hashes.json":
            assert hashlib.sha256(data).hexdigest() == index[member.name]
            payloads[member.name] = data
    vm = [name for name in names if name.endswith("/actual-case-exit")]
    native = [name for name in names
              if name.startswith("records/native-terminal-v1/") and name.endswith("/exit")]
    assert len(vm) == 26 and len(native) == 7
    for name in vm + native:
        assert payloads[name].strip() == b"0"
        stdout = (name.removesuffix("actual-case-exit") + "actual-case-stdout"
                  if name in vm else name.removesuffix("exit") + "stdout")
        assert b"test result: ok. 1 passed; 0 failed; 0 ignored;" in payloads[stdout]
        assert b"SKIP:" not in payloads[stdout]
    assert summary["source"] == "2c57ac6d7c410b90fc61a6f186a506eedde840ee"
    assert len(summary["continuation_vm"]) == len(vm)
    assert len(summary["continuation_native"]) == len(native)
    assert all(row["exit"] == 0 and row["executed_tests"] == 1
               for row in summary["continuation_vm"] + summary["continuation_native"])

print(json.dumps({
    "tested_source": summary["source"],
    "archive_sha256": hashlib.sha256(raw).hexdigest(),
    "verified_payloads": len(index),
    "verified_members": len(members),
    "additional_vm_cases": len(vm),
    "additional_native_cases": len(native),
    "current_main_pass": False,
    "limits": "Historical source only; no population/kernel/performance/management closure proof",
}, indent=2))
