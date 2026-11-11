#!/usr/bin/env python3
"""Freeze the reviewed third case in the same complete native VMM closure."""
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile


here = Path(__file__).resolve().parent
revision = sys.argv[1]
assert len(revision) == 40
base = here.parents[1] / "reserved-close-native-v1/package-v1/source-package.tar.gz"
raw = base.read_bytes()
base_sha = hashlib.sha256(raw).hexdigest()
assert base_sha == "37448e07bd227d19346d990d188bfe46ed1f0a43c1b789666262c82d7003970f"
files = {}
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    members = archive.getmembers()
    assert len({member.name for member in members}) == len(members)
    for member in members:
        path = PurePosixPath(member.name)
        assert member.isfile() and not path.is_absolute() and ".." not in path.parts
        assert str(path) == member.name and "\\" not in member.name
    index = json.loads(archive.extractfile("source-package-manifest.json").read())
    rows = {row["path"]: row for row in index["files"]}
    assert len(rows) == 176 and len(members) == len(rows) + 1
    for name, row in rows.items():
        member = archive.getmember(name)
        data = archive.extractfile(member).read()
        assert len(data) == row["bytes"]
        assert hashlib.sha256(data).hexdigest() == row["sha256"]
        files[name] = [data, member.mode]

leaf = "docs/research/measurements/managed-audit-wire-adversarial/native.rs"
new_leaf = subprocess.check_output(["git", "show", revision + ":" + leaf])
assert hashlib.sha256(new_leaf).hexdigest() == "65c73f963edef4ffb81754cf2eea2bd4ecd6a44d7f14178994725b211d0a4c65"
files["source/" + leaf][0] = new_leaf
builder_path = here.relative_to(Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())) / "build-native.py"
files["build-native.py"][0] = subprocess.check_output(["git", "show", revision + ":" + builder_path.as_posix()])
for name, (data, _) in files.items():
    if name.startswith("source/"):
        path = name.removeprefix("source/")
        if path == "Cargo.toml":
            continue
        tracked_path = "Cargo.toml" if path == ".source-original-Cargo.toml" else path
        assert data == subprocess.check_output(["git", "show", revision + ":" + tracked_path]), path
manifest = {
    "schema": 1,
    "source_revision": revision,
    "policy_revision": "7a897def2ac3bb041da584d1d6854fa066ad7fc2",
    "rules_sha256": "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3",
    "approved_patch_sha256": "77687ca0019061d41556c0dfd65319e7f7a30e41101dc144dfe83f63a5c560bb",
    "base_package_sha256": base_sha,
    "same_eight_real_crate_dependency_closure": True,
    "packaging_only": "Same eight real workspace members/default-members and unused patch removal as previous package; original manifest and lock retained; no stubs or VM runner",
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
           "new_native_cases_run": 0, "previous_sources_and_artifacts_unchanged": True}
with (output / "package-receipt.json").open("x") as file:
    json.dump(receipt, file, indent=2)
    file.write("\n")
print(json.dumps(receipt, indent=2))
