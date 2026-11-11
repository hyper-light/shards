#!/usr/bin/env python3
"""Prepare a private light harness; this is not the complete daemon caller."""

import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[4]
    destination = args.directory.resolve()
    destination.mkdir(parents=True, exist_ok=False)
    (destination / "src" / "daemon").mkdir(parents=True)

    path = root / "crates/shards/src/daemon/managed.rs"
    original = path.read_text()
    prefix, tail = original.split("impl<D: Disk> Daemon<D> {", 1)
    _, tail = tail.split("\nfn copy_labels(", 1)
    source = prefix + "\nfn copy_labels(" + tail
    prefix, tail = source.split("\nfn owner_key(", 1)
    _, tail = tail.split("\nfn record_capacity(", 1)
    source = prefix + "\nfn record_capacity(" + tail
    source = source.replace(
        "use super::{Daemon, Disk, Threads, lock, log};", "use super::{lock, log};"
    )
    source = source.replace(
        "#[cfg(test)]\nmod terminal_order_tests;\n", "",
    )
    source, _ = source.rsplit(
        "\n#[cfg(test)]\n#[allow(clippy::unwrap_used, clippy::panic)]\nmod tests {", 1
    )
    (destination / "src/daemon/managed.rs").write_text(source)
    (destination / "src/lib.rs").write_text(
        "#![allow(dead_code, unused_imports)]\n"
        "// Pure accounting fixture input; not a host capacity measurement.\n"
        "mod build { pub fn host_available() -> Option<u64> { Some(u64::MAX) } }\n"
        "mod daemon {\n"
        "  fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {\n"
        "    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)\n"
        "  }\n"
        "  fn log(_: String) {}\n"
        "  mod managed;\n"
        "}\n"
    )
    (destination / "tests").mkdir()
    boundary = (Path(__file__).parent / "producer-boundary.rs").read_text()
    observer = root / "crates/shards/tests/common/managed_capture.rs"
    (destination / "tests/producer_boundary.rs").write_text(
        boundary.replace("@OBSERVER_PATH@", json.dumps(str(observer)))
    )
    dependencies = {
        name: {"path": str(root / "crates" / directory)}
        for name, directory in [
            ("shards-abi", "abi"),
            ("shards-ipc", "ipc"),
            ("shards-remote", "remote"),
            ("shards-registry", "registry"),
            ("shards-vmm", "vmm"),
        ]
    }
    manifest = [
        '[package]',
        'name = "managed-resource-enforcement-light"',
        'version = "0.0.0"',
        'edition = "2024"',
        '[workspace]',
        '[dependencies]',
        'libc = "0.2"',
        'sha2 = "0.11"',
    ]
    manifest.extend(
        f"{name} = {{ path = {json.dumps(value['path'])} }}"
        for name, value in dependencies.items()
    )
    manifest.extend([
        '[dev-dependencies]',
        'hyper-quic = { git = "https://github.com/hyper-light/hyper-raft", rev = "233b7fcef7a363327c5439b191724a38065062cd", default-features = false }',
    ])
    manifest.append('[patch.crates-io]')
    for name in ["aws-lc-rs", "aws-lc-sys"]:
        manifest.append(f"{name} = {{ path = {json.dumps(str(root / 'vendor' / name))} }}")
    manifest.extend([
        '[lints.rust]',
        'unsafe_op_in_unsafe_fn = "deny"',
        '[lints.clippy]',
    ])
    for name in [
        "unwrap_used", "expect_used", "panic", "panic_in_result_fn", "todo",
        "unimplemented", "unreachable", "indexing_slicing", "string_slice",
    ]:
        manifest.append(f'{name} = "deny"')
    (destination / "Cargo.toml").write_text("\n".join(manifest) + "\n")
    product_lock = (root / "Cargo.lock").read_bytes()
    (destination / "Cargo.lock").write_bytes(product_lock)
    record = {
        "base": "0342b85d9c808e3ced041a6c3ef0e9bf70438272",
        "source": str(path),
        "source_sha256": hashlib.sha256(original.encode()).hexdigest(),
        "adapted_sha256": hashlib.sha256(source.encode()).hexdigest(),
        "product_lock_sha256": hashlib.sha256(product_lock).hexdigest(),
        "canonical_observer_sha256": hashlib.sha256(observer.read_bytes()).hexdigest(),
        "boundary_fixture_sha256": hashlib.sha256(boundary.encode()).hexdigest(),
        "excluded": [
            "Daemon startup impl", "owner key lookup",
            "unrelated owner-key/generation tests", "ROOT-only terminal-order fixtures",
        ],
        "fixture_inputs": ["host_available=u64::MAX for pure accounting only", "log sink"],
        "real_dependencies": dependencies,
        "not_proven": ["complete caller", "host authority", "kernel/VM accounting", "real transport"],
    }
    (destination / "source.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2))


if __name__ == "__main__":
    main()
