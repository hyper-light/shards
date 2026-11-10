#!/usr/bin/env python3
"""Prepare compile-only exact helper/fixture source with explicit host glue stubs."""
from pathlib import Path
import hashlib
import json
import re
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
target.mkdir()
src = target / "src"
src.mkdir()
owned = "docs/research/measurements/managed-tenant-exit"
host = repo / "crates/shards/tests/managed_output.rs"
guest = repo / "crates/testguest/src/workload.rs"
host_text, guest_text = host.read_text(), guest.read_text()
shutil.copyfile(repo / owned / "light-stubs.rs", src / "lib.rs")
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_exit.rs", src / "tenant_exit.rs")
(src / "payload.rs").write_text(
    host_text[host_text.index("fn payload("):host_text.index("struct Fixture")]
    + host_text[host_text.index("fn declared_label("):host_text.index("fn with_fixture(")]
)
(src / "helper.rs").write_text(
    guest_text[guest_text.index("fn managed_capture_bytes("):guest_text.index("/// Set by SIGUSR1")]
)
abi = target / "abi"
shutil.copytree(repo / "crates/abi/src", abi / "src")
shutil.copyfile(repo / "crates/abi/build.rs", abi / "build.rs")
(abi / "Cargo.toml").write_text('[package]\nname="shards-abi"\nversion="0.1.0"\nedition="2024"\n')
versions = dict(re.findall(r'name = "(libc|serde_json)"\nversion = "([^"]+)"', (repo / "Cargo.lock").read_text()))
(target / "Cargo.toml").write_text('''[package]
name="managed-tenant-exit-compile-only"
version="0.1.0"
edition="2024"
[dependencies]
libc="=''' + versions["libc"] + '''"
serde_json="=''' + versions["serde_json"] + '''"
shards-abi={path="abi"}
''')
hashes = {str(p.relative_to(target)): hashlib.sha256(p.read_bytes()).hexdigest()
          for p in sorted(target.rglob("*")) if p.is_file()}
(target / "source-hashes.json").write_text(json.dumps(hashes, indent=2) + "\n")
print(json.dumps({"scope":"compile-only; no host stub or helper execution", "files":hashes}, indent=2))
