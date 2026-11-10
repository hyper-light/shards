#!/usr/bin/env python3
"""Private admitted old/candidate policy comparison with explicit allocator calibration.

Own this checkout and compiler target exclusively. No production knob is added:
source variants, source patches, compiler artifacts and binary hashes are retained.
"""
from pathlib import Path
import hashlib
import json
import os
import resource
import shutil
import subprocess

ROOT = Path.cwd()
OUT = Path(os.environ['MANAGED_PAIRED_EVIDENCE'])
OUT.mkdir(parents=True, exist_ok=False)
OLD = 'f116ac276d2d53296de24c2a43837baf66ed1cbd'
NEW = '233b7fcef7a363327c5439b191724a38065062cd'
MANIFEST = Path('crates/remote/Cargo.toml')
LOCK = Path('Cargo.lock')
MAIN = Path('docs/research/measurements/managed-performance/src/main.rs')
OWNED = [MANIFEST, LOCK, MAIN]
ORIGINAL = {path: path.read_bytes() for path in OWNED}
REBUILD = [Path('crates/remote/src/managed.rs'), Path('crates/remote/src/net.rs'), MAIN,
           Path('docs/research/measurements/managed-performance/src/shared.rs'),
           Path('docs/research/measurements/managed-performance/net_stats.rs')]
assert all(NEW in ORIGINAL[path].decode() for path in [MANIFEST, LOCK])
assert 'MANAGED_TRACE_ALLOC_MIN' not in os.environ
(OUT / 'source.txt').write_bytes(subprocess.check_output(['git', 'rev-parse', 'HEAD']))
(OUT / 'environment.txt').write_bytes(subprocess.check_output(['uname', '-a']) + subprocess.check_output(['rustc', '-Vv']))


def parent_snapshot():
    usage = resource.getrusage(resource.RUSAGE_SELF)
    snapshot = dict(minor_faults=usage.ru_minflt, major_faults=usage.ru_majflt,
                    user_s=usage.ru_utime, system_s=usage.ru_stime,
                    maxrss_kib=usage.ru_maxrss, fds=len(os.listdir('/proc/self/fd')),
                    tasks=len(os.listdir('/proc/self/task')))
    for name in ['smaps_rollup', 'cgroup']:
        snapshot[name] = Path('/proc/self', name).read_text()
    return snapshot


def build(policy, counted):
    name = policy + ('-counted' if counted else '-system')
    directory = OUT / name
    directory.mkdir()
    for path, content in ORIGINAL.items():
        path.write_bytes(content)
    if policy == 'baseline':
        for path in [MANIFEST, LOCK]:
            path.write_text(path.read_text().replace(NEW, OLD))
    if not counted:
        source = MAIN.read_text()
        marker = '#[cfg(test)]\n#[global_allocator]\nstatic ALLOCATOR: Count = Count;'
        assert source.count(marker) == 1
        source = source.replace(marker, '#[cfg(test)]\n#[global_allocator]\nstatic ALLOCATOR: System = System;')
        marker = '    #[cfg(test)]\n    {\n        true\n    }'
        assert source.count(marker) == 1
        MAIN.write_text(source.replace(marker, '    #[cfg(test)]\n    {\n        false\n    }'))
    # kiln sync may preserve old mtimes. Every build touches only these owned
    # sources; rustc's artifact JSON must prove the measured crate is not fresh.
    for path in REBUILD:
        path.touch()
    (directory / 'source.patch').write_bytes(subprocess.check_output(['git', 'diff', '--binary']))
    (directory / 'hashes.json').write_text(json.dumps({str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in REBUILD + [MANIFEST, LOCK]}, indent=2))
    command = ['cargo', 'test', '--locked', '--release', '-j4', '-p', 'shards-remote', '--lib', '--no-run', '--message-format=json']
    (directory / 'command.json').write_text(json.dumps(command))
    with (directory / 'build.jsonl').open('wb') as output, (directory / 'build.stderr').open('wb') as error:
        subprocess.run(command, stdout=output, stderr=error, check=True, timeout=600)
    artifacts = [json.loads(line) for line in (directory / 'build.jsonl').read_text().splitlines()]
    binary_artifacts = [a for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target', {}).get('name') == 'shards_remote' and a.get('executable')]
    assert len(binary_artifacts) == 1 and not binary_artifacts[0]['fresh'], 'stale native artifact'
    artifact = Path(binary_artifacts[0]['executable'])
    binary = directory / 'measurement-binary'
    shutil.copy2(artifact, binary)
    (directory / 'binary.json').write_text(json.dumps(dict(original_path=str(artifact), sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), source_policy=policy, counted=counted), indent=2))
    print('built', name, flush=True)
    return binary


binaries = {}
try:
    for counted in [True, False]:
        for policy in ['baseline', 'candidate']:
            binaries[(policy, counted)] = build(policy, counted)
finally:
    for path, content in ORIGINAL.items():
        path.write_bytes(content)
        assert path.read_bytes() == content

# Five process pairs for counted cost, three pairs for allocator calibration;
# explicit fixture sample counts do not become runtime policy. All runs serial.
for counted, repetitions in [(True, 5), (False, 3)]:
    for repeat in range(repetitions):
        policies = ['baseline', 'candidate'] if repeat % 2 == 0 else ['candidate', 'baseline']
        variants = ['endpoint', 'shared'] if repeat % 2 == 0 else ['shared', 'endpoint']
        for tenants in [1, 2]:
            for mode in ['normal', 'stall']:
                for variant in variants:
                    for policy in policies:
                        prefix = OUT / (policy + ('-counted' if counted else '-system')) / f'r{repeat}-t{tenants}-{mode}-{variant}'
                        environment = dict(os.environ, MANAGED_ARGS=f'{tenants} 128 1024 10000 {mode} {variant}', MANAGED_HANDSHAKE_BYTES='16384', MANAGED_DATAGRAM_BYTES='65527')
                        (prefix.with_suffix('.parent-before.json')).write_text(json.dumps(parent_snapshot()))
                        command = ['/usr/bin/time', '-v', 'timeout', '120', str(binaries[(policy, counted)]), 'managed::performance::native_authenticated_comparison', '--ignored', '--exact', '--test-threads=1', '--nocapture']
                        with prefix.with_suffix('.tsv').open('wb') as output, prefix.with_suffix('.time').open('wb') as error:
                            result = subprocess.run(command, env=environment, stdout=output, stderr=error, timeout=130)
                        prefix.with_suffix('.exit').write_text(str(result.returncode) + '\n')
                        prefix.with_suffix('.parent-after.json').write_text(json.dumps(parent_snapshot()))
                        assert result.returncode == 0, str(prefix)
        print('paired repetition', counted, repeat, 'passed', flush=True)
(OUT / 'complete.json').write_text(json.dumps(dict(passing_processes=128, counted=80, system_allocator=48)))
