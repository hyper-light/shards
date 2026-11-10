#!/usr/bin/env python3
"""Prepare ONLY the pure retained-record fixture component and actual ABI dependency."""
from pathlib import Path
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
target.mkdir()
(target / "src").mkdir()
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_reconnect.rs", target / "src/tenant_reconnect.rs")
(target / "src/lib.rs").write_text('''#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
#[path="tenant_reconnect.rs"]
mod reconnect;
''')
abi = target / "abi"
shutil.copytree(repo / "crates/abi/src", abi / "src")
shutil.copyfile(repo / "crates/abi/build.rs", abi / "build.rs")
(abi / "Cargo.toml").write_text('[package]\nname="shards-abi"\nversion="0.1.0"\nedition="2024"\n')
(target / "Cargo.toml").write_text('''[package]
name="managed-tenant-observer-pure"
version="0.1.0"
edition="2024"
[dependencies]
shards-abi={path="abi"}
''')
shutil.copyfile(repo / "rustfmt.toml", target / "rustfmt.toml")
