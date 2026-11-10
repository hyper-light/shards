#!/usr/bin/env python3
"""Only the two before/after fake-muxer contracts and scoped library clippy."""
from pathlib import Path
import base64
import hashlib
import io
import json
import os
import shutil
import subprocess
import tarfile

root = Path.home() / 'native-borrowed-nowait-20261110-v1'
expected = {
    'before': '88296bbcf145c774d52a99f2fd46f0fc20c3655f0e9aaa2a78eedda5ef70d332',
    'after': 'c5fb92a41daca11da91ad3c4b97bd5cd16252daa84dba9e04195c7f8b258e796',
}
assert shutil.disk_usage(root).free >= 32 * 1024**3
for label, digest in expected.items():
    data = base64.b64decode((root/f'{label}-v3.tar.gz.b64').read_bytes(), validate=True)
    assert hashlib.sha256(data).hexdigest() == digest
    (root/f'{label}-v3.tar.gz').write_bytes(data)
    source = root/f'source-{label}-v3'
    source.mkdir()
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        for member in archive.getmembers():
            path = Path(member.name)
            assert member.isfile() and not path.is_absolute() and '..' not in path.parts
            dest = source/path
            dest.parent.mkdir(parents=True, exist_ok=True)
            with dest.open('xb') as out:
                out.write(archive.extractfile(member).read())
    for name, sha in json.loads((source/'source-hashes.json').read_text()).items():
        assert hashlib.sha256((source/name).read_bytes()).hexdigest() == sha, name

env = {k:v for k,v in os.environ.items() if not k.startswith('DYLD_')}
env.update(CARGO_HOME=str(root/'cargo-home'), CARGO_TARGET_DIR=str(root/'target-unused'),
           RUSTUP_TOOLCHAIN='1.98.1', CARGO_BUILD_JOBS='1', CARGO_INCREMENTAL='0',
           CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS='1')
results = {}
for label in ['before','after']:
    env['CARGO_TARGET_DIR']=str(root/f'target-{label}-v3')
    assert not Path(env['CARGO_TARGET_DIR']).exists()
    command = ['cargo','test','--locked','-j1','--profile','test-release','--message-format=json',
               '-p','shards-api','--lib','client::borrowed_tests::contract::','--',
               '--test-threads=1','--nocapture']
    (root/f'{label}-v3.command.json').write_text(json.dumps(command)+'\n')
    with (root/f'{label}-v3.log').open('wb') as log:
        run = subprocess.run(command, cwd=root/f'source-{label}-v3', env=env,
                             stdout=log, stderr=subprocess.STDOUT, timeout=300)
    text = (root/f'{label}-v3.log').read_text()
    artifacts = []
    for line in text.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'compiler-artifact' and item.get('executable'):
            path = Path(item['executable'])
            (root/'artifacts').mkdir(exist_ok=True)
            saved = root/'artifacts'/f'{label}-v3-api-unit'
            shutil.copyfile(path, saved)
            artifacts.append({'path':str(saved),'bytes':saved.stat().st_size,
                              'sha256':hashlib.sha256(saved.read_bytes()).hexdigest(),'cargo_fresh':item['fresh']})
    results[label] = {'exit':run.returncode,'artifacts':artifacts,
                      'ran_two': 'running 2 tests' in text,
                      'expected_contract_result': ('2 failed' in text if label=='before' else '2 passed' in text)}
    (root/'results-v3.json').write_text(json.dumps(results, indent=2)+'\n')
    print(label, json.dumps(results[label]), flush=True)
for name, selection, count in [
    ('focused-regressions-v3', ['client::borrowed_tests', '--skip', 'contract::'], 7),
    ('ready-io-v3', ['stream::tests::already_queued_data_is_read_before_waiting_out_a_timeout', '--exact'], 1),
]:
    command = ['cargo','test','--locked','-j1','--profile','test-release','-p','shards-api','--lib',
               selection[0], '--', *selection[1:], '--test-threads=1','--nocapture']
    (root/f'{name}.command.json').write_text(json.dumps(command)+'\n')
    with (root/f'{name}.log').open('wb') as log:
        run = subprocess.run(command, cwd=root/'source-after-v3', env=env,
                             stdout=log, stderr=subprocess.STDOUT, timeout=300)
    text = (root/f'{name}.log').read_text()
    results[name] = {'exit':run.returncode,'expected_count':count,
                     'expected_summary':f'{count} passed; 0 failed' in text}
    (root/'results-v3.json').write_text(json.dumps(results, indent=2)+'\n')
    print(name, json.dumps(results[name]), flush=True)
command = ['cargo','clippy','--locked','-j1','-p','shards-api','--lib','--','-D','warnings']
(root/'clippy-v3.command.json').write_text(json.dumps(command)+'\n')
with (root/'clippy-v3.log').open('wb') as log:
    run = subprocess.run(command, cwd=root/'source-after-v3', env=env,
                         stdout=log, stderr=subprocess.STDOUT, timeout=300)
results['clippy'] = {'exit':run.returncode}
results['storage'] = {'free_after_bytes':shutil.disk_usage(root).free}
(root/'results-v3.json').write_text(json.dumps(results, indent=2)+'\n')
with (root/'private-storage-v3.txt').open('w') as output:
    subprocess.run(['du','-sk',str(root)], stdout=output, check=True)
print(json.dumps(results), flush=True)
assert results['before']['exit']==101 and results['before']['ran_two'] and results['before']['expected_contract_result']
assert results['after']['exit']==0 and results['after']['ran_two'] and results['after']['expected_contract_result']
assert results['clippy']['exit']==0
assert results['before']['artifacts'][0]['sha256']!=results['after']['artifacts'][0]['sha256']
assert all(not a['cargo_fresh'] for k in ['before','after'] for a in results[k]['artifacts'])
assert all(results[k]['exit']==0 and results[k]['expected_summary'] for k in ['focused-regressions-v3','ready-io-v3'])
