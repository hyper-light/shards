from pathlib import Path
import hashlib
import json
import os
import stat
import subprocess


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def invoke(directory, label, argv, env=None):
    result = subprocess.run(argv, cwd=directory, env=env, capture_output=True, timeout=60)
    (root / (label + '.stdout')).write_bytes(result.stdout)
    (root / (label + '.stderr')).write_bytes(result.stderr)
    return {'argv': argv, 'cwd': str(directory), 'status': result.returncode,
            'stderr': result.stderr.decode(), 'stdout_sha256': hashlib.sha256(result.stdout).hexdigest()}


assert os.geteuid() != 0
root = Path.home() / 'root-kernel-config-mode-owner-write-v1'
root.mkdir(exist_ok=False)
frozen = Path.home() / 'root-kernel-config-mode-probe-v1' / 'frozen'
before = {p.name: {'sha256': digest(p), 'mode': stat.S_IMODE(p.stat().st_mode)}
          for p in frozen.iterdir()}
assert len(before) == 5 and all(x['mode'] == 0o444 for x in before.values())
base = frozen / 'firecracker-x86_64-6.18.config'
assert digest(base) == 'ba22401a0c7292a4c024ebcd10a562d4a1f1bfd2faed671406d3b159c0cf5215'
assert digest(frozen / 'merge_config.sh') == 'fecdeb0747c88f818f0d5d39333b948cdf83d9aca11c422253da17f5cc36e6f1'
env = dict(os.environ)
env['LC_ALL'] = 'C'
assert not env.get('KCONFIG_CONFIG')
record = {'uid': os.geteuid(), 'sources_before': before, 'minimum': [], 'exact_merge': [],
          'old_scopes_modified': False, 'kernel_compile': False, 'effective_kconfig_verified': False}

minimum = root / 'minimum'
minimum.mkdir()
source = minimum / 'merged.config'
source.write_bytes(b'')
target = minimum / '.config'
target.write_bytes(b'')
target.chmod(0o444)
for label, writable in [('red', False), ('green', True), ('restored_red', False)]:
    mode = stat.S_IMODE(target.stat().st_mode)
    target.chmod(mode | stat.S_IWUSR if writable else mode & ~stat.S_IWUSR)
    step = invoke(minimum, 'minimum_' + label, ['cp', '-T', '--', str(source), '.config'], env)
    step.update(label=label, mode=stat.S_IMODE(target.stat().st_mode),
                directory_writable=os.access(minimum, os.W_OK))
    record['minimum'].append(step)
    assert step['status'] == (0 if writable else 1)
    assert writable or 'Permission denied' in step['stderr']

for label, writable in [('red', False), ('green', True), ('restored_red', False)]:
    work = root / label
    work.mkdir()
    copied = invoke(work, label + '_copy', ['cp', str(base), '.config'], env)
    assert copied['status'] == 0
    config = work / '.config'
    assert digest(config) == digest(base)
    original_mode = stat.S_IMODE(config.stat().st_mode)
    assert original_mode == stat.S_IMODE(base.stat().st_mode)
    if writable:
        config.chmod(original_mode | stat.S_IWUSR)
    argv = ['sh', str(frozen / 'merge_config.sh'), '-m', '.config']
    argv.extend(str(frozen / n) for n in ['shards.config', 'shards-x86_64.config', 'managed-network.config'])
    step = invoke(work, label + '_merge', argv, env)
    step.update(label=label, original_mode=original_mode, working_mode=stat.S_IMODE(config.stat().st_mode),
                config_length=config.stat().st_size, config_sha256=digest(config))
    record['exact_merge'].append(step)
    assert step['status'] == (0 if writable else 1)
    if writable:
        assert digest(config) != digest(base)
        data = config.read_text()
        assert 'CONFIG_NFT_TPROXY=y' in data and 'CONFIG_NFT_SOCKET=y' in data
    else:
        assert 'Permission denied' in step['stderr'] and digest(config) == digest(base)

after = {p.name: {'sha256': digest(p), 'mode': stat.S_IMODE(p.stat().st_mode)}
         for p in frozen.iterdir()}
assert before == after
record.update(sources_unchanged=True, hypotheses={'copied_mode': 'confirmed',
              'directory_permissions': 'refuted', 'config_redirect': 'refuted'})
(root / 'RESULTS.json').write_text(json.dumps(record, indent=2) + '\n')
print(json.dumps({'root': str(root), 'minimum_statuses': [x['status'] for x in record['minimum']],
                  'exact_merge_statuses': [x['status'] for x in record['exact_merge']],
                  'sources_unchanged': record['sources_unchanged'], 'kernel_compile': False}))
