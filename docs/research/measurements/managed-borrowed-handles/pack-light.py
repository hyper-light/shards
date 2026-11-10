#!/usr/bin/env python3
"""Create a small compile-only bundle; never edits any API/parent source file.

Generated NEW lib_handles_fixture.rs/client_handles_fixture.rs add only module
registration in a private build context. ROOT owes actual parent registration
and full source-matching native compilation. The original API bytes are retained.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--source", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
root = args.source.resolve(strict=True)
args.output.mkdir()
owned = "docs/research/measurements/managed-borrowed-handles/"
paths = subprocess.run(["git", "ls-files", "--", "crates/api", "crates/abi", "crates/afunix", "crates/testdir"],
                       cwd=root, stdout=subprocess.PIPE, check=True).stdout.decode().splitlines()
paths += ["Cargo.lock", "Cargo.toml", "clippy.toml", ".cargo/config.toml", "AGENTS.md", "docs/development-rules.md",
          "docs/research/measurements/managed-native-borrowed/focused.rs"]
files = {"source/" + path: (root / path).read_bytes() for path in paths}
light_workspace = (root / "docs/research/measurements/managed-native-borrowed/light-workspace-Cargo.toml").read_bytes()
files["source/ORIGINAL-Cargo.toml"] = files["source/Cargo.toml"]
files["source/Cargo.toml"] = light_workspace
files["source/" + owned + "focused.rs"] = (root / owned / "focused.rs").read_bytes()
original_lib = files["source/crates/api/src/lib.rs"].decode()
assert original_lib.count("mod client;") == 1
files["source/crates/api/src/lib_handles_fixture.rs"] = original_lib.replace(
    "mod client;", '#[path = "client_handles_fixture.rs"]\nmod client;').encode()
client = files["source/crates/api/src/client.rs"]
files["source/crates/api/src/client_handles_fixture.rs"] = client + (
    '\n#[cfg(all(test, windows))]\n#[path = "../../../' + owned + 'focused.rs"]\nmod borrowed_handle_tests;\n').encode()
manifest = files["source/crates/api/Cargo.toml"].decode().replace('name = "shards-api"', 'name = "borrowed-handles-light"')
for name in ("abi", "afunix", "testdir"):
    manifest = manifest.replace('path = "../' + name + '"', 'path = "../source/crates/' + name + '"')
manifest += '\n[lib]\npath = "../source/crates/api/src/lib_handles_fixture.rs"\n[workspace]\nresolver = "3"\n'
manifest += "[workspace.package]" + light_workspace.decode().split("[workspace.package]", 1)[1]
files["fixture/Cargo.toml"] = manifest.encode()
files["fixture/Cargo.lock"] = files["source/Cargo.lock"]
files["fixture/clippy.toml"] = files["source/clippy.toml"]
files["fixture/.cargo/config.toml"] = files["source/.cargo/config.toml"]
identity = {"source_revision": subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, stdout=subprocess.PIPE, check=True).stdout.decode().strip(),
            "scope": "compile only, generated new module contexts; originals unchanged; native cases never run",
            "files": {name: {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()} for name, data in files.items()}}
files["BUNDLE-MANIFEST.json"] = (json.dumps(identity, indent=2) + "\n").encode()
archive = args.output / "compile-source.tar.gz"
with tarfile.open(archive, "w:gz") as target:
    for name, data in files.items():
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = 0o644
        target.addfile(info, io.BytesIO(data))
record = {"archive": archive.name, "bytes": archive.stat().st_size,
          "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(), "members": len(files),
          "input_and_generated": identity}
(args.output / "bundle.json").write_text(json.dumps(record, indent=2) + "\n")
print(json.dumps({key: record[key] for key in ("archive", "bytes", "sha256", "members")}))
