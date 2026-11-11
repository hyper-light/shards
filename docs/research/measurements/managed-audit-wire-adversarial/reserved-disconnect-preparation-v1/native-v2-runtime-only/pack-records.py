#!/usr/bin/env python3
"""After the sole command ends, verify/capture every actual runtime record."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import tarfile
import time

root = Path("/Users/adalundhe/managed-audit-wire-reserved-disconnect-runtime-v2")
assert (root / "controller/lock.result.json").is_file()
budget = json.loads((root / "budget.json").read_text())
identities = json.loads((root / "frozen-inputs/original-input-identities.json").read_text())
after = {}
for name, row in identities.items():
    path = Path(row["path"])
    data = path.read_bytes()
    assert len(data) == row["bytes"] and hashlib.sha256(data).hexdigest() == row["sha256"]
    copied = (root / row["copied_to"]).read_bytes()
    assert copied == data
    after[name] = {"sha256": row["sha256"], "bytes": len(data), "original_and_copy_unchanged": True}
with (root / "original-inputs-after.json").open("x") as file:
    json.dump(after, file, indent=2)
paths = [root / "budget.json", root / "corpus-verified.json", root / "original-inputs-after.json",
         root / "frozen-inputs/original-input-identities.json"]
for name in ["controller", "native-records-v2"]:
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
assert len(files) == len({row["path"] for row in files})
attempts = int((root / "controller/actor-start.json").exists())
manifest = {"schema": 1, "source_revision": budget["source_revision"],
            "preparation_revision": "31e665685b0b7111ecb89b5f2fbacae64939715a",
            "runner": budget["runner"], "native_case_attempts": attempts, "native_case_retries": 0,
            "budget": budget, "files": files,
            "preserved_outside_raw": ["artifacts/shards-vmm-native", "frozen-inputs/source-package.tar.gz",
                "frozen-inputs/source-package-manifest.json", "frozen-inputs/original-ready.json",
                "frozen-inputs/verify-corpus.py", "frozen-inputs/original-corpus-records", "input", "native-tmp-v2",
                "entire original expired v1 scope"]}
encoded = (json.dumps(manifest, indent=2) + "\n").encode()
with (root / "raw-manifest-v2.json").open("xb") as file:
    file.write(encoded)
archive_path = root / "raw-runtime-v2.tar.gz"
with tarfile.open(archive_path, "x:gz") as archive:
    for row in files:
        archive.add(root / row["path"], arcname=row["path"], recursive=False)
    member = tarfile.TarInfo("manifest.json")
    member.size, member.mode = len(encoded), 0o644
    archive.addfile(member, io.BytesIO(encoded))
raw = archive_path.read_bytes()
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
receipt = {"status": "AllActualRawVerified_NoActorRerun", "archive": str(archive_path), "bytes": len(raw),
           "sha256": hashlib.sha256(raw).hexdigest(), "manifest_sha256": hashlib.sha256(encoded).hexdigest(),
           "payloads": len(files), "total_members": len(files) + 1, "runtime_attempts": attempts,
           "runtime_retries": 0, "packed_at_epoch": time.time(), "scope_deadline_epoch": budget["deadline_epoch"]}
with (root / "raw-pack-receipt-v2.json").open("x") as file:
    json.dump(receipt, file, indent=2)
print(json.dumps(receipt), flush=True)
