#!/usr/bin/env python3
"""Extract exact ABI and existing pure reconnect checks; no host/transport stand-ins."""
from pathlib import Path
import hashlib
import json
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
target.mkdir()
shutil.copytree(repo / "crates/abi/src", target / "abi/src")
shutil.copyfile(repo / "crates/abi/build.rs", target / "abi/build.rs")
host = (repo / "crates/shards/tests/managed_output.rs").read_text()
exit_source = (repo / "crates/shards/tests/managed_output/tenant_exit.rs").read_text()
observer = (repo / "crates/shards/tests/managed_output/tenant_observer.rs").read_text()
retained = (repo / "crates/shards/tests/managed_output/tenant_reconnect.rs").read_text()
retained = retained.replace('#[path = "tenant_observer.rs"]\nmod fixture;\n\n', "", 1)
(target / "retained.rs").write_text(retained)
regions = {
    "capture": exit_source[exit_source.index("#[derive(Default)]"):exit_source.index("#[derive(Clone, Copy)]")],
    "payload": host[host.index("fn payload("):host.index("struct Fixture")],
    "label": host[host.index("fn declared_label("):host.index("fn with_fixture(")],
    "accept": observer[observer.index("enum Accepted {"):observer.index("fn receive(")],
    "reader_tests": observer[observer.index("#[cfg(test)]\nmod reader_tests {"):],
}
(target / "pure.rs").write_text(
    '#![allow(dead_code)]\n'
    'use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};\n'
    'use std::path::PathBuf;\n'
    '#[path = "retained.rs"] mod reconnect;\n'
    'use reconnect::RetainedRecord;\n'
    + "\n".join(regions.values())
)
hashes = {str(p.relative_to(target)): hashlib.sha256(p.read_bytes()).hexdigest()
          for p in sorted(target.rglob("*")) if p.is_file()}
(target / "source-hashes.json").write_text(json.dumps(hashes, indent=2) + "\n")
(target / "region-hashes.json").write_text(json.dumps({
    name: hashlib.sha256(text.encode()).hexdigest() for name, text in regions.items()
}, indent=2) + "\n")
print(json.dumps({"scope": "five exact pure checks only; full host/transport not compiled or run", "files": len(hashes)}))
