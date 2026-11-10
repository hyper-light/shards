#!/usr/bin/env python3
"""Preserve only this comparison's light logs and identities; no executable transfer."""
import hashlib
import io
import json
from pathlib import Path
import tarfile

home = Path.home()
names = ['managed-relay-precommit-proof-v2']
files = {}
binaries = []
for name in names:
    proof = home / name
    assert proof.is_dir(), proof
    for source in sorted(proof.rglob('*')):
        if not source.is_file() or source.is_symlink():
            continue
        relative = source.relative_to(proof)
        if 'target' in relative.parts:
            if source.name.startswith(('sock_addr_pure-', 'sock_addr_host_pure-')) and source.suffix == '':
                data = source.read_bytes()
                if data[:4] == b'\x7fELF':
                    binaries.append({'path': str(source), 'bytes': len(data),
                                     'sha256': hashlib.sha256(data).hexdigest()})
            continue
        # Full source snapshots and binaries remain in their original private dirs.
        # Source manifests identify them; signed source commits remain authoritative.
        if relative.parts[0] == 'source' or source.name in ['fixture.rs', 'cancel.rs']:
            continue
        data = source.read_bytes()
        assert data[:4] not in [b'\x7fELF', b'\xcf\xfa\xed\xfe', b'\xfe\xed\xfa\xcf'] and data[:2] != b'MZ'
        files[str(Path(name) / relative)] = data
files['binary-identities.json'] = (json.dumps(binaries, indent=2) + '\n').encode()
manifest = {name: hashlib.sha256(data).hexdigest() for name, data in files.items()}
files['SHA256SUMS.json'] = (json.dumps(manifest, indent=2) + '\n').encode()
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
    for name, data in sorted(files.items()):
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = 0o644
        archive.addfile(info, io.BytesIO(data))
data = buffer.getvalue()
path = home / 'managed-relay-precommit-light-v2.tgz'
path.write_bytes(data)
print(json.dumps({'path': str(path), 'bytes': len(data),
                  'sha256': hashlib.sha256(data).hexdigest(), 'entries': len(files),
                  'binary_identities': len(binaries)}))
