#!/usr/bin/env python3
"""ROOT bounded IPC region checks and one actual mapping-inheritance mutation.

No audit composition, native helper/actor/VM/cgroup/host-security mutation.
"""
from pathlib import Path, PurePosixPath
import base64
import hashlib
import json
import os
import resource
import select
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import zlib

BUDGET = 600
CLEANUP = 15
began = float(os.environ['ROOT_REGIONS_BEGAN'])
deadline = began + BUDGET
package = Path(sys.argv[1])
scope = Path(sys.argv[2])
assert os.uname().sysname == 'Linux' and os.uname().machine == 'x86_64'
assert os.geteuid() != 0 and time.monotonic() < deadline - CLEANUP
assert shutil.disk_usage(scope.parent).free >= 4 * 1024**3
scope.mkdir(mode=0o700)
records = scope / 'records'
records.mkdir()
spec = json.loads(zlib.decompress(package.read_bytes()))
manifest = spec['source_manifest']
encoded = (json.dumps(manifest, sort_keys=True, indent=2) + '\n').encode()
assert hashlib.sha256(encoded).hexdigest() == spec['source_manifest_sha256']
template = Path.home() / 'Projects/managed-native-owner-runtime-01a1c99e-v8'
assert hashlib.sha256((template / 'source/FROZEN-SOURCE.json').read_bytes()).hexdigest() == spec['template_manifest_sha256']

def digest(data):
    return hashlib.sha256(data).hexdigest()

def identity(path):
    data = path.read_bytes()
    return {'bytes': len(data), 'sha256': digest(data)}

def inventory(root):
    result = {}
    for path in sorted(root.rglob('*')):
        assert not path.is_symlink(), str(path)
        if path.is_file():
            result[str(path.relative_to(root))] = identity(path)
    return result

source = scope / 'source'
source.mkdir()
template_used = {}
for name, expected in manifest['files'].items():
    rel = PurePosixPath(name)
    assert not rel.is_absolute() and '..' not in rel.parts and '\\' not in name
    if expected['kind'] == 'symlink':
        continue  # Preserved as metadata, never followed or extracted.
    if name in spec['overlay_base64']:
        data = base64.b64decode(spec['overlay_base64'][name], validate=True)
    else:
        old = template / 'source' / name
        assert old.is_file() and not old.is_symlink()
        data = old.read_bytes()
        template_used[name] = identity(old)
    assert len(data) == expected['bytes'] and digest(data) == expected['sha256'], name
    path = source / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    path.chmod(0o755 if expected['executable'] else 0o644)
(source / 'ROOT-SOURCE-MANIFEST.json').write_bytes(encoded)
(records / 'source-before.json').write_text(json.dumps({'manifest_sha256': spec['source_manifest_sha256'], 'regular_files': len(manifest['files']) - len(manifest['omitted_symlinks']), 'omitted_links': manifest['omitted_symlinks'], 'template_used': template_used}, sort_keys=True, indent=2) + '\n')

cargo = scope / 'cargo-home'
cargo.mkdir()
cache_before = {}
for name in ['registry', 'git']:
    old = template / 'cargo-home' / name
    cache_before[name] = inventory(old)
    shutil.copytree(old, cargo / name, symlinks=False)
temporary = scope / 'tmp'
temporary.mkdir()
allowed = sorted(os.sched_getaffinity(0))
assert len(allowed) >= 2
os.sched_setaffinity(0, set(allowed[:2]))
environment = os.environ.copy()
assert not environment.get('LD_PRELOAD')
environment.update(CARGO_HOME=str(cargo), CARGO_TARGET_DIR=str(scope / 'target'), TMPDIR=str(temporary), CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0')
shutil.copy2(__file__, records / 'driver.py')
(records / 'identity.json').write_text(json.dumps({'uname': list(os.uname()), 'uid': os.geteuid(), 'cpus': sorted(os.sched_getaffinity(0)), 'normal_envelope': '2CPU/2GiB/4GiB/600s including15s cleanup/j2', 'limits_before': {'address_space': list(resource.getrlimit(resource.RLIMIT_AS)), 'memlock': list(resource.getrlimit(resource.RLIMIT_MEMLOCK))}, 'scope': str(scope), 'source_manifest_sha256': spec['source_manifest_sha256'], 'actual_main': spec['main_revision'], 'forbidden': 'No held audit graph/five, helper, actor, VM, cgroups, host-root/security or pressure campaigns'}, indent=2) + '\n')

def child_limits():
    old = resource.getrlimit(resource.RLIMIT_AS)
    cap = 2 * 1024**3
    resource.setrlimit(resource.RLIMIT_AS, tuple(cap if value == resource.RLIM_INFINITY else min(value, cap) for value in old))

def stage(name, command, limit=180, cwd=source, extra=None, expected=0):
    remaining = deadline - CLEANUP - time.monotonic()
    if remaining <= 0:
        raise RuntimeError('original absolute deadline expired before ' + name)
    before = len(os.listdir('/proc/self/fd'))
    env = environment.copy()
    if extra:
        env.update(extra)
    (records / (name + '.command.json')).write_text(json.dumps(command, indent=2) + '\n')
    timeout = False
    with (records / (name + '.stdout')).open('wb') as out, (records / (name + '.stderr')).open('wb') as err:
        child = subprocess.Popen(command, cwd=cwd, env=env, stdout=out, stderr=err, start_new_session=True, preexec_fn=child_limits)
        pidfd = os.pidfd_open(child.pid, 0)
        try:
            child.wait(timeout=min(limit, remaining))
        except subprocess.TimeoutExpired:
            timeout = True
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=CLEANUP)
        poll = select.poll()
        poll.register(pidfd, select.POLLIN)
        ready = bool(poll.poll(0))
        os.close(pidfd)
        try:
            os.waitpid(child.pid, os.WNOHANG)
            reaped = False
        except ChildProcessError:
            reaped = True
    receipt = {'command': command, 'exit': child.returncode, 'timeout': timeout, 'pidfd_ready': ready, 'direct_child_waited_ECHILD': reaped, 'fd_before': before, 'fd_after': len(os.listdir('/proc/self/fd')), 'elapsed': time.monotonic() - began, 'descendant_population': 'Unknown'}
    (records / (name + '.json')).write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'stage': name, 'exit': child.returncode, 'timeout': timeout, 'ready': ready, 'fd': [receipt['fd_before'], receipt['fd_after']]}), flush=True)
    assert ready and reaped and receipt['fd_before'] == receipt['fd_after']
    if timeout or child.returncode != expected:
        raise RuntimeError(name + ' stopped at first actual failure')

def compiled_binary(stdout):
    binaries = []
    for line in stdout.read_text().splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if value.get('reason') == 'compiler-artifact' and value.get('target', {}).get('name') == 'shards_ipc' and value.get('profile', {}).get('test') and value.get('executable'):
            binaries.append(Path(value['executable']))
    assert len(binaries) == 1, binaries
    return binaries[0]

target = 'x86_64-unknown-linux-gnu'
common = ['--locked', '--offline', '-p', 'shards-ipc', '--target', target, '-j2']
cases = [
    'locked_region::tests::acquired_region_is_initialized_locked_writable_and_explicitly_released',
    'locked_region::tests::region_fork_guard_prevents_mapping_inheritance_and_reaps_the_probe',
    'locked_region::tests::unrepresentable_region_refuses_before_mapping_or_locking',
]
code = 0
failure = None
binary = None
mutant = None
try:
    stage('policy', [sys.executable, 'scripts/check-development-policy'])
    stage('owned-format', ['rustfmt', '--edition', '2024', '--config-path', 'rustfmt.toml', '--check', 'crates/ipc/src/locked_region.rs'])
    stage('ipc-check', ['cargo', 'check', *common, '--all-targets'])
    stage('ipc-test-build', ['cargo', 'test', *common, '--no-run', '--message-format=json'])
    binary = compiled_binary(records / 'ipc-test-build.stdout')
    artifacts = scope / 'artifacts'
    artifacts.mkdir()
    frozen = artifacts / binary.name
    shutil.copy2(binary, frozen)
    binary = frozen
    (records / 'precase-binary.json').write_text(json.dumps({'path': str(binary), **identity(binary), 'source_manifest_sha256': spec['source_manifest_sha256']}, indent=2) + '\n')
    stage('ipc-test-list', [str(binary), '--list'], limit=15)
    listing = (records / 'ipc-test-list.stdout').read_text()
    for case in cases:
        assert listing.splitlines().count(case + ': test') == 1
    for index, case in enumerate(cases):
        stage('new-region-case-' + str(index), [str(binary), '--exact', case, '--nocapture', '--test-threads=1'], limit=15)
        assert '1 passed; 0 failed; 0 ignored;' in (records / ('new-region-case-' + str(index) + '.stdout')).read_text()
    stage('ipc-all-target-strict', ['cargo', 'clippy', *common, '--all-targets', '--', '-D', 'warnings'])
    # One exact guard removal; no other case is replayed in the mutant.
    mutant = scope / 'mutant-source'
    shutil.copytree(source, mutant, symlinks=False)
    leaf = mutant / 'crates/ipc/src/locked_region.rs'
    data = leaf.read_bytes()
    assert data.count(b'libc::MADV_DONTFORK') == 1
    leaf.write_bytes(data.replace(b'libc::MADV_DONTFORK', b'libc::MADV_DOFORK'))
    (records / 'mutation.json').write_text(json.dumps({'path': str(leaf), 'before': digest(data), 'after': identity(leaf), 'change': 'ONE production madvise DONTFORK -> DOFORK; actual mapping observer must detect inheritance'}, indent=2) + '\n')
    stage('mutant-test-build', ['cargo', 'test', *common, '--no-run', '--message-format=json'], cwd=mutant, extra={'CARGO_TARGET_DIR': str(scope / 'mutant-target')})
    mutant_binary = compiled_binary(records / 'mutant-test-build.stdout')
    mutant_frozen = artifacts / ('mutant-' + mutant_binary.name)
    shutil.copy2(mutant_binary, mutant_frozen)
    (records / 'precase-mutant-binary.json').write_text(json.dumps({'path': str(mutant_frozen), **identity(mutant_frozen)}, indent=2) + '\n')
    stage('mutant-inheritance-case', [str(mutant_frozen), '--exact', cases[1], '--nocapture', '--test-threads=1'], limit=15, cwd=mutant, expected=101)
    observed = (records / 'mutant-inheritance-case.stdout').read_text()
    assert 'process_refused=1 mapping_absent=0' in observed
    assert '0 passed; 1 failed; 0 ignored;' in observed
    stage('restored-guard-case', [str(binary), '--exact', cases[1], '--nocapture', '--test-threads=1'], limit=15)
    assert 'process_refused=1 mapping_absent=1' in (records / 'restored-guard-case.stdout').read_text()
except Exception as error:
    code = 1
    failure = {'type': type(error).__name__, 'message': str(error)}
finally:
    unchanged = 0
    for name, expected in manifest['files'].items():
        if expected['kind'] == 'symlink':
            continue
        verify = identity(source / name)
        assert verify['bytes'] == expected['bytes'] and verify['sha256'] == expected['sha256'], name
        unchanged += 1
    for name, expected in template_used.items():
        assert identity(template / 'source' / name) == expected, name
    for name, expected in cache_before.items():
        assert inventory(template / 'cargo-home' / name) == expected, name
    final = {'exit': code, 'failure': failure, 'elapsed_seconds': time.monotonic() - began, 'original_budget': BUDGET, 'unchanged_source_files': unchanged, 'unchanged_template_files': len(template_used), 'unchanged_cache_files': {name: len(files) for name, files in cache_before.items()}, 'new_cases_planned': 3, 'mutation': 'one private mapping-inheritance guard only', 'held_scope_dispatches': 0, 'actual_kernel_ancestor_backing': 'Unproved'}
    if binary:
        final['frozen_binary'] = identity(binary)
    (records / 'final.json').write_text(json.dumps(final, indent=2) + '\n')
    payloads = {str(p.relative_to(records)): identity(p) for p in sorted(records.rglob('*')) if p.is_file()}
    (records / 'MANIFEST.json').write_text(json.dumps(payloads, sort_keys=True, indent=2) + '\n')
    archive = scope / 'raw.tar.gz'
    with tarfile.open(archive, 'w:gz') as tar:
        for p in sorted(records.iterdir()):
            tar.add(p, arcname=p.name, recursive=False)
    print(json.dumps({'final': final, 'archive': str(archive), **identity(archive)}), flush=True)
sys.exit(code)
