#!/usr/bin/env python3
"""Copy ROOT's accepted reconnect checks without loading any live fixture body."""
from pathlib import Path
import hashlib
import json
import subprocess
import sys

repo, destination = map(Path, sys.argv[1:])
accepted = "54188c747f2a64db7e1de564cd70df0bd16ca5af"
policy = "cbd39284937d024640c56db6f8d8ef2bd9b0ee24"


def show(revision, path):
    return subprocess.check_output(["git", "show", f"{revision}:{path}"], cwd=repo)


def put(path, data):
    target = destination / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(data)


destination.mkdir()
abi = subprocess.check_output(
    ["git", "ls-tree", "-r", "--name-only", policy, "crates/abi"], cwd=repo, text=True
).splitlines()
for path in abi:
    if not (path.startswith("crates/abi/src/") or path == "crates/abi/build.rs"):
        continue
    current = show(policy, path)
    assert current == show(accepted, path), f"ABI changed: {path}"
    put("abi/" + path.removeprefix("crates/abi/"), current)

sources = {}
for name in ["managed_output.rs", "tenant_exit.rs", "tenant_reconnect.rs", "tenant_observer.rs"]:
    path = "crates/shards/tests/" + (name if name == "managed_output.rs" else "managed_output/" + name)
    data = show(accepted, path)
    put("original/" + name, data)
    sources[name] = data.decode()

host = sources["managed_output.rs"]
exit_source = sources["tenant_exit.rs"]
observer = sources["tenant_observer.rs"]
retained = sources["tenant_reconnect.rs"]
declaration = '#[path = "tenant_observer.rs"]\nmod fixture;\n\n'
assert retained.count(declaration) == 1
put("retained.rs", retained.replace(declaration, "", 1).encode())
regions = {
    "capture": exit_source[exit_source.index("#[derive(Default)]"):exit_source.index("#[derive(Clone, Copy)]")],
    "payload": host[host.index("fn payload("):host.index("struct Fixture")],
    "label": host[host.index("fn declared_label("):host.index("fn with_fixture(")],
    "accept": observer[observer.index("enum Accepted {"):observer.index("fn receive(")],
    "reader_tests": observer[observer.index("#[cfg(test)]\nmod reader_tests {"):],
}
pure = (
    '#![allow(dead_code)]\n'
    'use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};\n'
    'use std::path::PathBuf;\n'
    '#[path = "retained.rs"] mod reconnect;\n'
    'use reconnect::RetainedRecord;\n'
    + "\n".join(regions.values())
)
assert "fn run(" not in pure and "fn receive(" not in pure
put("pure.rs", pure.encode())
put("rustfmt.toml", show(policy, "rustfmt.toml"))
for path in [
    "AGENTS.md", "CLAUDE.md", "docs/development-rules.md", "docs/development-evidence.md",
    "scripts/check-development-policy", "scripts/lint", "Cargo.toml",
    "resources/supported-targets.txt", ".github/workflows/ci.yml", ".github/workflows/release.yml",
]:
    put("policy/" + path, show(policy, path))
rules_sha = hashlib.sha256(show(policy, "docs/development-rules.md")).hexdigest()
assert rules_sha == "df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
put("approved-composition.md", show(accepted, "docs/research/measurements/managed-coordinator-reconnect/COMPOSITION.md"))
put("region-hashes.json", (json.dumps({
    key: hashlib.sha256(value.encode()).hexdigest() for key, value in regions.items()
}, indent=2) + "\n").encode())
manifest = {str(p.relative_to(destination)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(destination.rglob("*")) if p.is_file()}
put("source-manifest.json", (json.dumps({
    "accepted_source": accepted, "policy_revision": policy, "policy_sha256": rules_sha,
    "scope": "five exact retained-record/reader pure tests; live fixture bodies excluded",
    "abi_equal_to_current_main": True, "files": manifest,
}, indent=2) + "\n").encode())
print(json.dumps({"files": len(manifest), "bytes": sum(p.stat().st_size for p in destination.rglob("*") if p.is_file())}))
