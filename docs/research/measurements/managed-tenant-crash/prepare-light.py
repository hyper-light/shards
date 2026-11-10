#!/usr/bin/env python3
"""Prepare exact helper/host fixture bodies for compile-only validation; never execute them."""
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
prior = repo / "docs/research/measurements/managed-tenant-exit"
host = (repo / "crates/shards/tests/managed_output.rs").read_text()
guest = (repo / "crates/testguest/src/workload.rs").read_text()
stubs = (prior / "light-stubs.rs").read_text()
stubs = stubs.replace("_: &str, _: Instant)", "_: &str, _: Option<u32>, _: Instant)")
stubs = stubs.replace('mod helper {\n    #![allow(dead_code)]', '''mod helper {
    #![allow(dead_code)]
    #![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,
            clippy::indexing_slicing, clippy::string_slice)]''')
(src / "lib.rs").write_text(stubs)
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_exit.rs", src / "tenant_exit.rs")
(src / "payload.rs").write_text(
    host[host.index("fn payload("):host.index("struct Fixture")]
    + host[host.index("fn declared_label("):host.index("fn with_fixture(")])
(src / "helper.rs").write_text(
    guest[guest.index("fn managed_capture_bytes("):guest.index("/// Set by SIGUSR1")])
original = target / "original"
original.mkdir()
shutil.copyfile(repo / "crates/shards/tests/managed_output.rs", original / "managed_output.rs")
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_exit.rs", original / "tenant_exit.rs")
shutil.copyfile(repo / "rustfmt.toml", target / "rustfmt.toml")
abi = target / "abi"
shutil.copytree(repo / "crates/abi/src", abi / "src")
shutil.copyfile(repo / "crates/abi/build.rs", abi / "build.rs")
(abi / "Cargo.toml").write_text('[package]\nname="shards-abi"\nversion="0.1.0"\nedition="2024"\n')
versions = dict(re.findall(r'name = "(libc|serde_json)"\nversion = "([^"]+)"', (repo / "Cargo.lock").read_text()))
(target / "Cargo.toml").write_text('''[package]
name="managed-tenant-crash-compile-only"
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
print(json.dumps({"scope":"compile-only; no host stub/helper/test body executed", "files":hashes}, indent=2))
