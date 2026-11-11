#!/usr/bin/env python3
"""Freeze the same full VMM dependency closure with the approved test leaf."""
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile


here = Path(__file__).parent
base = here.parent / "native-v2/source-package.tar.gz"
raw = base.read_bytes()
base_sha = hashlib.sha256(raw).hexdigest()
assert base_sha == "c11862f9d9b8c40dbab9125380a4b61aab9dcc15ea6cf71e20207808348c9c31"
revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
assert revision == "82e7243104aaf15b70a373d49fcdf4293136e012"
files = {}
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    index = json.loads(archive.extractfile("source-package-manifest.json").read())
    rows = {row["path"]: row for row in index["files"]}
    assert len(rows) == 176
    for name, row in rows.items():
        member = archive.getmember(name)
        data = archive.extractfile(member).read()
        assert member.isfile() and len(data) == row["bytes"]
        assert hashlib.sha256(data).hexdigest() == row["sha256"]
        files[name] = [data, member.mode]

leaf = "docs/research/measurements/managed-audit-wire-adversarial/native.rs"
new_leaf = subprocess.check_output(["git", "show", revision + ":" + leaf])
assert hashlib.sha256(new_leaf).hexdigest() == "228bb9e67e1e699d969a71de817c5a9c6e461ee18e07d98aa070029b07b59e4e"
files["source/" + leaf][0] = new_leaf
files["build-native.py"][0] = (here / "build-native.py").read_bytes()
for name, (data, _) in files.items():
    if name.startswith("source/"):
        path = name.removeprefix("source/")
        # These are the already disclosed packaging-only root manifest choices;
        # every product crate file and tracked original lock remains exact Git.
        if path in ["Cargo.toml", ".source-original-Cargo.toml"]:
            continue
        assert data == subprocess.check_output(["git", "show", revision + ":" + path]), path
manifest = {
    "schema": 1,
    "source_revision": revision,
    "policy_revision": "b6af4ec8efa246c8a12fb44abd10425a5adc9af5",
    "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
    "base_package_sha256": base_sha,
    "same_eight_real_crate_dependency_closure": True,
    "packaging_only": "Same root workspace member/default-member closure and unused patch removal as original native package; original Cargo.toml retained; no stubs or VM runner",
    "files": [{"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
              for name, (data, _) in sorted(files.items())],
}
encoded = (json.dumps(manifest, indent=2) + "\n").encode()
output = here / "package-v1"
output.mkdir()
with (output / "source-package-manifest.json").open("xb") as file:
    file.write(encoded)
with tarfile.open(output / "source-package.tar.gz", "x:gz") as archive:
    for name, (data, mode) in sorted(files.items()):
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = mode
        archive.addfile(info, io.BytesIO(data))
    info = tarfile.TarInfo("source-package-manifest.json")
    info.size = len(encoded)
    info.mode = 0o644
    archive.addfile(info, io.BytesIO(encoded))
result = (output / "source-package.tar.gz").read_bytes()
receipt = {"source_revision": revision, "package_bytes": len(result),
           "package_sha256": hashlib.sha256(result).hexdigest(),
           "manifest_sha256": hashlib.sha256(encoded).hexdigest(),
           "payload_count": len(files), "fixture_sha256": hashlib.sha256(new_leaf).hexdigest(),
           "new_native_cases_run": 0, "previous_source_and_artifact_unchanged": True}
with (output / "package-receipt.json").open("x") as file:
    json.dump(receipt, file, indent=2)
    file.write("\n")
print(json.dumps(receipt, indent=2))
