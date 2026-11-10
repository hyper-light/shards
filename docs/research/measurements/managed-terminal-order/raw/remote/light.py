#!/usr/bin/env python3
"""Generate a compile-only workspace from exact daemon fragments and real crates.

This is not the complete daemon. The only behavior scaffold is host_available()
returning None (admission is unused); real registry Quantum, remote Control,
Shared, Endpoint, driver and all fixture methods are type-checked, never run.
The coordinator still owes complete-daemon compilation and all native cases.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re


def between(source, first, next_anchor):
    if source.count(first) != 1:
        raise ValueError(f"expected one source anchor: {first!r}")
    start = source.index(first)
    return source[start:source.index(next_anchor, start + len(first))]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def generate(source, output):
    source = source.resolve(strict=True)
    output.mkdir(parents=False, exist_ok=False)
    paths = {
        "daemon": "crates/shards/src/daemon/managed.rs",
        "parent": "crates/shards/src/daemon.rs",
        "leaf": "crates/shards/src/daemon/managed/terminal_order_tests.rs",
        "remote": "crates/remote/src/managed.rs",
        "cargo": "Cargo.toml",
    }
    text = {key: (source / path).read_text() for key, path in paths.items()}
    fragments = {
        "admission": between(text["daemon"], "#[derive(Debug)]\npub(super) enum Admission", "\nimpl std::fmt::Display for Failure"),
        "charge_output": between(text["daemon"], "struct Charge {", "\nstruct Owner {"),
        "credits": between(text["daemon"], "#[derive(Clone, Copy)]\nenum After", "\nimpl Outputs"),
        "lock": between(text["parent"], "fn lock<T>(m: &Mutex<T>)", "\n/// A warm VM"),
        "log": between(text["parent"], "fn log(message: impl std::fmt::Display)", "\n/// The lines the daemons"),
    }
    generated = """// Exact daemon fragments; compile-only scaffold, never a complete daemon.
#![allow(dead_code)]
mod build {
    // Admission is unused by the fixture. This compile scaffold cannot admit.
    pub fn host_available() -> Option<u64> { None }
}
mod daemon {
use std::io::{self, Write as _};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
static LOGGED: AtomicU64 = AtomicU64::new(0);
""" + fragments["lock"] + "\n" + fragments["log"] + """
mod managed {
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use shards_abi::run::managed::Channel;
use shards_remote::identity;
use shards_remote::managed::{Admitted, Control, Endpoint, Producer, Progress, RunId, Scope, State};
use super::{lock, log};
"""
    handshake = re.findall(r"^const HANDSHAKE_BYTES: usize = .*;$", text["daemon"], re.MULTILINE)
    if len(handshake) != 1:
        raise ValueError("expected actual handshake bound")
    generated += handshake[0] + "\n" + "\n".join(fragments[key] for key in ("admission", "charge_output", "credits"))
    generated += "\n#[cfg(test)]\n#[path = " + json.dumps(str(source / paths["leaf"])) + "]\nmod terminal_order_tests;\n}\n}\n"
    (output / "lib.rs").write_text(generated)
    cargo = text["cargo"]
    lints = between(cargo, "[workspace.lints.rust]", "\n# The TLS provider")
    package = between(cargo, "[workspace.package]", "\n# Versions are pinned")
    manifest = """[package]
name = "terminal-order-light"
version = "0.0.0"
edition = "2024"
[lib]
path = "lib.rs"
[dependencies]
"""
    for name, path in (("shards-abi", "abi"), ("shards-remote", "remote"), ("shards-registry", "registry")):
        manifest += name + " = { path = " + json.dumps(str(source / "crates" / path)) + " }\n"
    manifest += "libc = \"0.2\"\n[lints]\nworkspace = true\n[workspace]\nresolver = \"3\"\n" + package + "\n" + lints
    manifest += "\n[patch.crates-io]\n"
    for name in ("aws-lc-rs", "aws-lc-sys", "rquickjs-sys"):
        manifest += name + " = { path = " + json.dumps(str(source / "vendor" / name)) + " }\n"
    (output / "Cargo.toml").write_text(manifest)
    (output / "clippy.toml").write_bytes((source / "clippy.toml").read_bytes())
    config = output / ".cargo"
    config.mkdir()
    (config / "config.toml").write_bytes((source / ".cargo/config.toml").read_bytes())
    for name, fragment in fragments.items():
        (output / f"{name}.fragment").write_text(fragment)
    identity = {
        "source": {paths[key]: digest(value.encode()) for key, value in text.items()},
        "fragments": {name: digest(value.encode()) for name, value in fragments.items()},
        "generated_lib_sha256": digest(generated.encode()),
        "generated_manifest_sha256": digest(manifest.encode()),
        "scope": "compile-only exact daemon fragments + actual remote/registry dependencies; inert host_available; native cases never run",
    }
    (output / "source.json").write_text(json.dumps(identity, indent=2) + "\n")
    print(json.dumps(identity))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    generate(args.source, args.output)
