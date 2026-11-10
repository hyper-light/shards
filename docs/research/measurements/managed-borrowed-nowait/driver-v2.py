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
    'before': '585cff4356429e50377efcc896001086feddb95bc2347bbe1398015315ba1edc',
    'after': 'd45b750d9534ce8331febf96ccb6f693b98c381285235c6fbc659df317c7754e',
}
assert shutil.disk_usage(root).free >= 32 * 1024**3
for label, digest in expected.items():
    data = (root/f'{label}.tar.gz').read_bytes()
    assert hashlib.sha256(data).hexdigest() == digest
    source = root/f'source-{label}'
    for name, sha in json.loads((source/'source-hashes.json').read_text()).items():
        assert hashlib.sha256((source/name).read_bytes()).hexdigest() == sha, name

env = {k:v for k,v in os.environ.items() if not k.startswith('DYLD_')}
env.update(CARGO_HOME=str(root/'cargo-home'), CARGO_TARGET_DIR=str(root/'target-unused'),
           RUSTUP_TOOLCHAIN='1.98.1', CARGO_BUILD_JOBS='1', CARGO_INCREMENTAL='0',
           CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS='1')
results = {}
for label in ['before','after']:
    env['CARGO_TARGET_DIR']=str(root/f'target-{label}-v2')
    assert not Path(env['CARGO_TARGET_DIR']).exists()
    command = ['cargo','test','--locked','-j1','--profile','test-release','--message-format=json',
               '-p','shards-api','--lib','client::borrowed_tests::contract::','--',
               '--test-threads=1','--nocapture']
    (root/f'{label}-v2.command.json').write_text(json.dumps(command)+'\n')
    with (root/f'{label}-v2.log').open('wb') as log:
        run = subprocess.run(command, cwd=root/f'source-{label}', env=env,
                             stdout=log, stderr=subprocess.STDOUT, timeout=300)
    text = (root/f'{label}-v2.log').read_text()
    artifacts = []
    for line in text.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'compiler-artifact' and item.get('executable'):
            path = Path(item['executable'])
            (root/'artifacts').mkdir(exist_ok=True)
            saved = root/'artifacts'/f'{label}-v2-api-unit'
            shutil.copyfile(path, saved)
            artifacts.append({'path':str(saved),'bytes':saved.stat().st_size,
                              'sha256':hashlib.sha256(saved.read_bytes()).hexdigest(),'cargo_fresh':item['fresh']})
    results[label] = {'exit':run.returncode,'artifacts':artifacts,
                      'ran_two': 'running 2 tests' in text,
                      'expected_contract_result': ('2 failed' in text if label=='before' else '2 passed' in text)}
    (root/'results-v2.json').write_text(json.dumps(results, indent=2)+'\n')
    print(label, json.dumps(results[label]), flush=True)
command = ['cargo','clippy','--locked','-j1','-p','shards-api','--lib','--','-D','warnings']
(root/'clippy-v2.command.json').write_text(json.dumps(command)+'\n')
with (root/'clippy-v2.log').open('wb') as log:
    run = subprocess.run(command, cwd=root/'source-after', env=env,
                         stdout=log, stderr=subprocess.STDOUT, timeout=300)
results['clippy'] = {'exit':run.returncode}
results['storage'] = {'free_after_bytes':shutil.disk_usage(root).free}
(root/'results-v2.json').write_text(json.dumps(results, indent=2)+'\n')
with (root/'private-storage-v2.txt').open('w') as output:
    subprocess.run(['du','-sk',str(root)], stdout=output, check=True)
print(json.dumps(results), flush=True)
assert results['before']['exit']==101 and results['before']['ran_two'] and results['before']['expected_contract_result']
assert results['after']['exit']==0 and results['after']['ran_two'] and results['after']['expected_contract_result']
assert results['clippy']['exit']==0
assert results['before']['artifacts'][0]['sha256']!=results['after']['artifacts'][0]['sha256']
assert all(not a['cargo_fresh'] for k in ['before','after'] for a in results[k]['artifacts'])
