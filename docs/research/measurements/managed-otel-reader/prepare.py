#!/usr/bin/env python3
"""Assemble a private light harness from exact real sources; never alter them."""

import argparse
import hashlib
import json
from pathlib import Path


READER = "8f35e059201b73618fa23cfd3f33a394253a701cc9520e267950513873e9f1e5"
CODEC = "9aa16e1df9b84c4cad69744e50a44f93be66d83afd59642304d010d0b85de354"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", type=Path)
    parser.add_argument("reader", type=Path)
    parser.add_argument("codec", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    sources = {}

    def copy(source, target, expected=None):
        data = source.read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if expected is not None and digest != expected:
            raise ValueError(f"{source}: expected {expected}, found {digest}")
        destination = args.output / target
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        sources[target] = {"source": str(source), "sha256": digest, "bytes": len(data)}

    copy(args.reader, "src/metrics.rs", READER)
    copy(args.codec, "src/metrics/otlp.rs", CODEC)
    copy(args.repository / "crates/init/src/metrics/tests.rs", "src/tests.rs")
    for relative in ["Cargo.toml", "src/lib.rs", "src/bin/shards-testhelper.rs"]:
        copy(args.repository / "crates/testdir" / relative, "testdir/" + relative)
    copy(args.repository / "clippy.toml", "clippy.toml")
    copy(args.repository / "rustfmt.toml", "rustfmt.toml")
    copy(args.repository / "rust-toolchain.toml", "rust-toolchain.toml")
    manifest = (args.repository / "Cargo.toml").read_text()
    package = manifest.split("[workspace.package]\n", 1)[1].split("\n[", 1)[0]
    lints = manifest.split("[workspace.lints.rust]\n", 1)[1].split("\n# The TLS", 1)[0]
    (args.output / "Cargo.toml").write_text(
        '[package]\nname = "managed-otel-reader-light"\nversion = "0.0.0"\n'
        'edition.workspace = true\npublish = false\n'
        '[dependencies]\nlibc.workspace = true\n'
        '[dev-dependencies]\nshards-testdir = { path = "testdir" }\n'
        '[workspace]\nresolver = "3"\nmembers = ["testdir"]\n'
        '[workspace.package]\n' + package + '\n'
        '[workspace.dependencies]\nlibc = "=0.2.189"\n'
        '[workspace.lints.rust]\n' + lints + '\n'
    )
    (args.output / "src/lib.rs").write_text(
        'pub mod metrics;\n'
        '#[cfg(test)]\nuse metrics::Reader;\n'
        '#[cfg(test)]\nuse metrics::otlp::{Failure, Scope, Snapshot};\n'
        '#[cfg(test)]\n#[path = "tests.rs"]\nmod reader_tests;\n'
    )
    (args.output / "source-map.json").write_text(json.dumps(sources, indent=2) + "\n")
    print(json.dumps({"output": str(args.output), "sources": sources}, indent=2))


if __name__ == "__main__":
    main()
