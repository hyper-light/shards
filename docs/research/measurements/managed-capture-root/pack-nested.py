"""Archive ROOT's completed native ARM64 KVM gate without changing originals."""
from pathlib import Path, PurePosixPath
import hashlib
import io
import json
import subprocess
import tarfile

original = Path('/Users/adalundhe/root-managed-capture-nested-09a-v1')
source = Path('/Users/adalundhe/Projects/shards/.claude/worktrees/managed-capture-nested-root-v1')
destination = Path(__file__).parent / 'root-records/nested-capture-09a-v2'
destination.mkdir(exist_ok=False)
revision = '09a187d2a55522d59a79b0fc21ac9378f25c0fb9'
assert subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip() == revision
assert not subprocess.check_output(['git', '-C', str(source), 'status', '--porcelain', '--untracked-files=no'])
identity = json.loads((original / 'identity.json').read_bytes())
for name, digest in identity['source_sha256'].items():
    assert hashlib.sha256((source / name).read_bytes()).hexdigest() == digest
assert json.loads((original / 'completed.json').read_bytes())['ordinary_kvm_arm64'] == '25passed/0failed/10ignored'

payloads = {p.name: p.read_bytes() for p in original.iterdir() if p.is_file() and not p.is_symlink()}
artifacts = []
for line in (original / 'host-boot-carried-build.stdout').read_text().splitlines():
    try:
        record = json.loads(line)
    except ValueError:
        continue
    if record.get('executable'):
        artifacts.append(Path(record['executable']))
artifacts.extend((source / 'target/artifacts').iterdir())
linux = source / 'target/l1/aarch64-unknown-linux-musl/test-release'
artifacts.append(linux / 'shards')
artifacts.extend(p for p in (linux / 'deps').glob('managed_output-*') if p.suffix != '.d')
assert len(artifacts) == 6
artifact_hashes = []
for path in artifacts:
    assert path.is_file() and not path.is_symlink()
    with path.open('rb') as stream:
        digest = hashlib.sha256()
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
        digest = digest.hexdigest()
    artifact_hashes.append({'path': str(path), 'bytes': path.stat().st_size, 'sha256': digest})
payloads['retained-artifacts.json'] = (json.dumps(artifact_hashes, indent=2) + '\n').encode()
members = [{'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()} for name, data in sorted(payloads.items())]
manifest = {'source': revision, 'original': str(original), 'members': members,
            'excluded_retained': ['target/', 'empty-docker/', str(source / 'target')],
            'whole_population_kernel_return': 'Unknown'}
payloads['manifest.json'] = (json.dumps(manifest, indent=2) + '\n').encode()
archive = destination / 'raw.tar.gz'
with tarfile.open(archive, 'x:gz') as output:
    for name, data in sorted(payloads.items()):
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = 0o644
        output.addfile(info, io.BytesIO(data))

with tarfile.open(archive, 'r:gz') as packed:
    entries = packed.getmembers()
    names = [entry.name for entry in entries]
    assert len(names) == len(set(names)) == len(payloads)
    for entry in entries:
        name = PurePosixPath(entry.name)
        assert entry.isfile() and not name.is_absolute() and '..' not in name.parts
        assert packed.extractfile(entry).read() == payloads[entry.name]
    packed_manifest = json.loads(packed.extractfile('manifest.json').read())
    assert packed_manifest == manifest
    for member in packed_manifest['members']:
        data = packed.extractfile(member['path']).read()
        assert len(data) == member['bytes']
        assert hashlib.sha256(data).hexdigest() == member['sha256']
verification = {'source': revision, 'archive_bytes': archive.stat().st_size,
                'archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
                'verified_payloads': len(members), 'verified_members': len(payloads),
                'all_members_safe_unique_regular_exact': True,
                'ordinary_kvm_arm64': '25passed/0failed/10ignored',
                'whole_population_kernel_return': 'Unknown'}
(destination / 'member-hashes.json').write_text(json.dumps(manifest, indent=2) + '\n')
(destination / 'verified.json').write_text(json.dumps(verification, indent=2) + '\n')
print(json.dumps(verification))
