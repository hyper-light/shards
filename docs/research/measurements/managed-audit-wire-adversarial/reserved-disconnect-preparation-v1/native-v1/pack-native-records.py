#!/usr/bin/env python3
"""Preserve completed build/case files; never starts actors or extends a budget."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import tarfile
import time

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-v1")
budget = json.loads((root / "budget.json").read_text())
paths = [root / "budget.json", root / "corpus-verified.json"]
for name in ["build-records", "case-controller-v1", "native-records-v1"]:
    if (root / name).exists():
        paths += sorted((root / name).rglob("*"))
files = []
for path in paths:
    if path.is_dir():
        continue
    assert path.is_file() and not path.is_symlink()
    raw = path.read_bytes()
    files.append({"path": path.relative_to(root).as_posix(), "bytes": len(raw),
                  "sha256": hashlib.sha256(raw).hexdigest()})
assert len({row["path"] for row in files}) == len(files)
attempts = int((root / "case-controller-v1/actor-start.json").exists())
manifest = {"schema": 1, "source_revision": budget["source_revision"], "runner": budget["runner"],
            "native_case_attempts": attempts, "native_case_retries": 0,
            "original_deadline_epoch": budget["deadline_epoch"], "files": files,
            "exclusions_preserved_on_runner": ["source", "target", "cargo-home", "artifacts/shards-vmm-native",
                "input", "original-corpus-records", "native-tmp-v1", "source-package.tar.gz",
                "source-package-manifest.json", "build-native.py"]}
encoded = (json.dumps(manifest, indent=2) + "\n").encode()
with (root / "raw-manifest-v1.json").open("xb") as file:
    file.write(encoded)
output = root / "raw-native-v1.tar.gz"
with tarfile.open(output, "x:gz") as archive:
    for row in files:
        archive.add(root / row["path"], arcname=row["path"], recursive=False)
    member = tarfile.TarInfo("manifest.json")
    member.size, member.mode = len(encoded), 0o644
    archive.addfile(member, io.BytesIO(encoded))
raw = output.read_bytes()
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    members = archive.getmembers()
    assert len(members) == len(files) + 1 and len({member.name for member in members}) == len(members)
    assert all(member.isfile() and str(PurePosixPath(member.name)) == member.name
               and not PurePosixPath(member.name).is_absolute() and ".." not in PurePosixPath(member.name).parts
               and "\\" not in member.name for member in members)
    assert archive.extractfile("manifest.json").read() == encoded
    for row in files:
        data = archive.extractfile(row["path"]).read()
        assert len(data) == row["bytes"] and hashlib.sha256(data).hexdigest() == row["sha256"]
receipt = {"status": "AllActualRawVerified_NoRerun", "archive": str(output), "bytes": len(raw),
           "sha256": hashlib.sha256(raw).hexdigest(), "manifest_sha256": hashlib.sha256(encoded).hexdigest(),
           "payloads": len(files), "total_members": len(files) + 1, "native_case_attempts": attempts,
           "runtime_retries": 0, "packed_at_epoch": time.time(),
           "original_deadline_epoch": budget["deadline_epoch"]}
with (root / "raw-pack-receipt-v1.json").open("x") as file:
    json.dump(receipt, file, indent=2)
print(json.dumps(receipt), flush=True)
