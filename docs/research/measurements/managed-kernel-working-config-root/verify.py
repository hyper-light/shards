from pathlib import Path
import hashlib
import io
import json
import tarfile

root = Path(__file__).resolve().parent
blob = (root / 'actual-mode-raw-v1.tar.gz').read_bytes()
assert len(blob) == 125894
assert hashlib.sha256(blob).hexdigest() == '134991aefc33983cb588d45b46f88dff510ef358efaa6c164d6434ecc96d6828'
with tarfile.open(fileobj=io.BytesIO(blob), mode='r:gz') as archive:
    members = archive.getmembers()
    names = [member.name for member in members]
    assert len(names) == len(set(names)) == 41
    assert all(member.isfile() and not member.name.startswith('/')
               and '..' not in Path(member.name).parts for member in members)
    manifest = json.loads(archive.extractfile('ORIGINAL-FILES.json').read())
    assert len(manifest) == 40
    assert manifest == json.loads((root / 'ORIGINAL-FILES.json').read_text())
    for name, identity in manifest.items():
        content = archive.extractfile(name).read()
        assert len(content) == identity['length']
        assert hashlib.sha256(content).hexdigest() == identity['sha256']
    result = json.loads(archive.extractfile('root-kernel-config-mode-owner-write-v1/RESULTS.json').read())
    for scenario in ['minimum', 'exact_merge']:
        assert [step['status'] for step in result[scenario]] == [1, 0, 1]
        assert [step['label'] for step in result[scenario]] == ['red', 'green', 'restored_red']
    assert result['sources_unchanged'] and not result['old_scopes_modified']
    assert not result['kernel_compile'] and not result['effective_kconfig_verified']
    assert len(result['sources_before']) == 5
    assert all(identity['mode'] == 0o444 for identity in result['sources_before'].values())
    for name, identity in result['sources_before'].items():
        data = archive.extractfile('root-kernel-config-mode-probe-v1/frozen/' + name).read()
        assert hashlib.sha256(data).hexdigest() == identity['sha256']
print('40 raw payloads verified; minimum and exact merge each RED1/GREEN0/restoredRED1; no kernel proof')
