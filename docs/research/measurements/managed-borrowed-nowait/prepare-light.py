#!/usr/bin/env python3
"""Package exact small API workspace; baseline changes only the test-harness path."""
from pathlib import Path
import hashlib
import json
import shutil
import subprocess
import sys
import tarfile
import re

repo, target = map(Path, sys.argv[1:3])
revision = sys.argv[3] if len(sys.argv) > 3 else None
target.mkdir()
roots = ["crates/abi", "crates/afunix", "crates/api", "crates/testdir"]
files = subprocess.check_output(["git", "ls-tree", "-r", "--name-only", revision or "HEAD", "--", *roots], cwd=repo, text=True).splitlines()
for name in files:
    dest = target / name
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(subprocess.check_output(["git", "show", f"{revision}:{name}"], cwd=repo) if revision else (repo/name).read_bytes())
old = repo / "docs/research/measurements/managed-native-borrowed"
(target / "Cargo.toml").write_bytes((old / "light-workspace-Cargo.toml").read_bytes())
light = (old / "raw/light-Cargo.lock").read_bytes()
def packages(text):
    records = {}
    for block in text.split("[[package]]")[1:]:
        name = re.search(r'^name = "([^"]+)"', block, re.MULTILINE).group(1)
        version = re.search(r'^version = "([^"]+)"', block, re.MULTILINE).group(1)
        records[(name, version)] = block.strip()
    return records
original = packages((repo / "Cargo.lock").read_text())
for identity, package in packages(light.decode()).items():
    assert package == original[identity], identity
(target / "Cargo.lock").write_bytes(light)
for name in ["rust-toolchain.toml", "rustfmt.toml", "clippy.toml", "docs/research/measurements/managed-proc-main/allocation.rs"]:
    dest=target/name
    dest.parent.mkdir(parents=True,exist_ok=True)
    shutil.copyfile(repo/name,dest)
new=Path("docs/research/measurements/managed-borrowed-nowait")
for name in ["focused.rs","contract.rs"]:
    dest=target/new/name
    dest.parent.mkdir(parents=True,exist_ok=True)
    shutil.copyfile(repo/new/name,dest)
if revision:
    client=target/"crates/api/src/client.rs"
    text=client.read_text().replace("managed-native-borrowed/focused.rs", "managed-borrowed-nowait/focused.rs")
    client.write_text(text)
    focused=target/new/"focused.rs"
    text=(old/"focused.rs").read_text()
    focused.write_text(text[:text.index("#[test]")] + '#[path = "contract.rs"]\nmod contract;\n')
hashes={str(p.relative_to(target)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(target.rglob('*')) if p.is_file()}
(target/"source-hashes.json").write_text(json.dumps(hashes,indent=2)+'\n')
archive=target.with_suffix('.tar.gz')
with tarfile.open(archive,'w:gz') as tar:
    for path in sorted(target.rglob('*')):
        if path.is_file(): tar.add(path,arcname=str(path.relative_to(target)))
print(json.dumps({'scope':'small actual API workspace; exact original lock package closure; not full workspace','revision':revision or 'current owned source','archive':str(archive),'bytes':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'files':len(hashes)}))
