#!/usr/bin/env python3
"""Freeze the actual server dependency graph at original repository paths."""
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[4]
OUT = Path(__file__).resolve().parent / 'closure-v3'
BASE = '8b1abae44d35ad7d82be296fc6cb15e3358fc5a3'


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT)


paths = [
    'Cargo.toml', 'Cargo.lock', 'clippy.toml', 'rustfmt.toml',
    'crates/server', 'crates/abi', 'crates/ipc', 'crates/testdir',
    'crates/init/src/json.rs',
    'vendor/aws-lc-rs', 'vendor/aws-lc-sys', 'vendor/rquickjs-sys',
    'docs/research/measurements/managed-program-admission/allocation.rs',
    'docs/research/measurements/managed-audit-producer/inflight-journal-v1/portable-tests.rs',
    'docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/canonical-peer-catalog.json',
    'docs/research/measurements/managed-harness-adapters/raw/claude-mcp-causal-01/native.stdout',
    'docs/research/measurements/managed-harness-adapters/raw/native-05/codex.stdout',
]
archive = git('archive', '--format=tar', BASE, '--', *paths)
files = {}
with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
    for member in tar:
        if member.isdir():
            continue
        rel = Path(member.name)
        if not member.isfile() or rel.is_absolute() or '..' in rel.parts or member.name in files:
            raise ValueError('nonregular, duplicate or unsafe source: ' + member.name)
        files[member.name] = (tar.extractfile(member).read(), member.mode, BASE)

# Only workspace membership changes. Original package manifests, lock, dependency
# versions, vendored sources, profiles and all Rust/Clippy lints remain exact.
name = 'Cargo.toml'
original = files[name][0].decode()
members = ['crates/server', 'crates/abi', 'crates/ipc', 'crates/testdir']
projected, n = re.subn(r'(?ms)^members = \[.*?^\]', 'members = ' + json.dumps(members), original, count=1)
if n != 1:
    raise ValueError('workspace membership not found')
projected, n = re.subn(r'(?m)^default-members = .*$', 'default-members = ["crates/server"]', projected, count=1)
if n != 1:
    raise ValueError('default membership not found')
files[name] = (projected.encode(), 0o644, BASE + ': membership-only projection')

for name in ['crates/server/src/native_effects/write.rs', 'crates/server/src/native_effects/write/tests.rs']:
    files[name] = ((ROOT / name).read_bytes(), 0o644, 'owned source at 198bd plus exact test-only14693 proposal')
name = 'crates/server/src/native_effects/mod.rs'
before = files[name][0]
if b'mod write' in before:
    raise ValueError('unexpected existing registration')
files[name] = (before + b'\npub(crate) mod write;\n', 0o644, BASE + ': ONE private module registration')

rows = []
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w') as tar:
    for name, (body, mode, origin) in sorted(files.items()):
        entry = tarfile.TarInfo(name)
        entry.size, entry.mode, entry.mtime = len(body), mode, 0
        tar.addfile(entry, io.BytesIO(body))
        rows.append(dict(path=name, bytes=len(body), sha256=hashlib.sha256(body).hexdigest(), origin=origin))
data = gzip.compress(buffer.getvalue(), mtime=0)
with (OUT / 'source.tar.gz').open('xb') as output:
    output.write(data)
manifest = dict(base=BASE, production_write_revision='198bd7a111c4c9929a7fa027be6344efdde8400c',
                files=rows, archive_bytes=len(data), archive_sha256=hashlib.sha256(data).hexdigest(),
                scope='Actual full server TEST compiler graph at original paths; only seven writer cases may execute. No held native_write record module in this older exact graph. No authority/caller/integration gate.')
with (OUT / 'source.json').open('x') as output:
    json.dump(manifest, output, indent=2)
    output.write('\n')
print(json.dumps(dict(files=len(rows), archive_bytes=len(data), archive_sha256=manifest['archive_sha256'])))
