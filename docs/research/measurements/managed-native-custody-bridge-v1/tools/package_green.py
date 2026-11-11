import base64, hashlib, json, shutil, sys, tarfile, zlib
from pathlib import Path
m = Path(sys.argv[1]); r8 = Path(sys.argv[2])
sys.path.insert(0, str(m / 'tools'))
from diagnostics import headings
RUN = 'crates/init/src/run.rs'
sha = lambda b: hashlib.sha256(b).hexdigest()
green = (m / 'variants/green/run.rs').read_bytes()
target = m / 'candidate-green'
shutil.copytree(m / 'candidate-red', target, symlinks=True)
(target / RUN).write_bytes(green)
manifest = json.loads((target / 'FROZEN-SOURCE.json').read_text())
manifest['meaning'] = 'ROOT GREEN: v10d custody plus typed Clone/Pid/OwnerParent refusals answered SYSTEM_ERR with NOT_RUN while the original failure stays owned until power_off; TWO new cases'
manifest['policy'] = {'revision': 'bff3015182ad754274f1048ce393ac12cb6cd91f', 'rules_sha256': 'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3', 'role': 'coordinator', 'scope': 'new ordinary ROOT source/tests only; RED-v2 and custody MUTANT variants in the same bounded job; no held audit graph/five/helper/actor/VM'}
manifest['files'][RUN].update(bytes=len(green), sha256=sha(green))
manifest['files'] = dict(sorted(manifest['files'].items(), key=lambda x: Path(x[0])))
frozen = (json.dumps(manifest, indent=2) + '\n').encode()
(target / 'FROZEN-SOURCE.json').write_bytes(frozen)
for name, pin in manifest['files'].items():
    p = target / name
    data = str(p.readlink()).encode() if p.is_symlink() else p.read_bytes()
    assert len(data) == pin['bytes'] and sha(data) == pin['sha256'], name
archive = m / 'source-green.tar.gz'
with tarfile.open(archive, 'w:gz') as t:
    for name in list(manifest['files']) + ['FROZEN-SOURCE.json']:
        t.add(target / name, arcname=name, recursive=False)
spec = json.loads(zlib.decompress((m / 'source-red-delta.zlib').read_bytes()))
spec['files'][RUN].update(bytes=len(green), sha256=sha(green), base64=base64.b64encode(green).decode())
spec.update(new_source_meaning=manifest['meaning'], new_policy=manifest['policy'],
            new_source_manifest_sha256=sha(frozen), local_full_archive_sha256=sha(archive.read_bytes()))
spec['variants'] = {}
for name in ['green', 'red-v2', 'mutant-custody']:
    data = (m / 'variants' / name / 'run.rs').read_bytes()
    spec['variants'][name] = {'bytes': len(data), 'sha256': sha(data), 'base64': base64.b64encode(data).decode()}
assert spec['variants']['green']['sha256'] == spec['files'][RUN]['sha256']
strict = (r8 / 'actual-init-strict.stderr').read_text()
spec['strict_baseline'] = headings(strict)
spec['strict_baseline_source'] = {'run': 'r8 immutable custody v10d 9904', 'stderr_sha256': sha(strict.encode()), 'count': len(spec['strict_baseline'])}
package = zlib.compress(json.dumps(spec, separators=(',', ':')).encode(), 9)
(m / 'source-green-delta.zlib').write_bytes(package)
summary = {'manifest_sha256': sha(frozen), 'full_source_sha256': spec['local_full_archive_sha256'],
           'package_sha256': sha(package), 'package_bytes': len(package), 'source_files': len(manifest['files']),
           'variants': {k: v['sha256'] for k, v in spec['variants'].items()}, 'strict_baseline_count': len(spec['strict_baseline'])}
(m / 'SOURCE-GREEN.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
