#!/usr/bin/env python3
"""Finite native checks; retained source/logs, no VM or resource-performance claim."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

source = Path.cwd()
scope = Path(os.environ['SHARDS_EFFECT_AUDIT_SCOPE'])
records = scope / os.environ.get('SHARDS_EFFECT_AUDIT_RECORDS', 'records-v1')
records.mkdir(parents=True, exist_ok=False)
env = dict(os.environ, CARGO_HOME=str(scope / 'cargo-home'), CARGO_TARGET_DIR=str(scope / 'target'),
           CARGO_BUILD_JOBS='2', ZIG_GLOBAL_CACHE_DIR=str(scope / 'zig-global'),
           ZIG_LOCAL_CACHE_DIR=str(scope / 'zig-local'))
for key in list(env):
    if key.startswith('DYLD_'):
        del env[key]

def run(name, command):
    (records / (name + '.command.json')).write_text(json.dumps(command))
    with (records / (name + '.stdout')).open('wb') as out, (records / (name + '.stderr')).open('wb') as err:
        result = subprocess.run(command, env=env, stdout=out, stderr=err, timeout=480)
    (records / (name + '.exit')).write_text(str(result.returncode) + '\n')
    print(name, result.returncode, flush=True)
    if result.returncode:
        print((records / (name + '.stderr')).read_text()[-12000:], flush=True)
    return result.returncode

broker = source / 'crates/server/src/broker.rs'
candidate = broker.read_bytes()
server = broker.parent / 'server.rs'
candidate_server = server.read_bytes()
(records / 'candidate-broker.rs').write_bytes(candidate)
(records / 'candidate-server.rs').write_bytes((broker.parent / 'server.rs').read_bytes())
(records / 'candidate-audit.rs').write_bytes((broker.parent / 'broker/audit.rs').read_bytes())
base = subprocess.check_output(['git', 'show', '0342b85d9c808e3ced041a6c3ef0e9bf70438272:crates/server/src/broker.rs'])
(records / 'baseline-broker.rs').write_bytes(base)
base_server = subprocess.check_output(['git', 'show', '0342b85d9c808e3ced041a6c3ef0e9bf70438272:crates/server/src/server.rs'])
(records / 'baseline-server.rs').write_bytes(base_server)
(records / 'source.json').write_text(json.dumps({
    'base': '0342b85d9c808e3ced041a6c3ef0e9bf70438272',
    'candidate_broker_sha256': hashlib.sha256(candidate).hexdigest(),
    'candidate_audit_sha256': hashlib.sha256((broker.parent / 'broker/audit.rs').read_bytes()).hexdigest(),
    'candidate_server_sha256': hashlib.sha256((broker.parent / 'server.rs').read_bytes()).hexdigest(),
    'baseline_broker_sha256': hashlib.sha256(base).hexdigest(),
    'uname': subprocess.check_output(['uname', '-a'], text=True).strip(),
    'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
    'scope': str(scope), 'source': str(source),
}, indent=2))
target = os.environ.get('SHARDS_EFFECT_AUDIT_TARGET')
target_args = ['--target', target] if target else []
common = ['cargo', 'test', '-p', 'shards-server', '--locked', '--profile', 'test-release', '-j2'] + target_args
result = 0
try:
    if os.environ.get('SHARDS_EFFECT_AUDIT_BASELINE') == 'skip':
        pass
    else:
        broker.write_bytes(base)
        server.write_bytes(base_server)
        result = run('baseline-required-refusal', common + ['broker_required_durable_refuses', '--', '--nocapture', '--test-threads=1'])
finally:
    broker.write_bytes(candidate)
    server.write_bytes(candidate_server)
    assert hashlib.sha256(broker.read_bytes()).digest() == hashlib.sha256(candidate).digest()
if result:
    sys.exit(result)
result = run('candidate-broker', common + ['broker_', '--', '--nocapture', '--test-threads=1'])
if not result:
    result = run('candidate-held-file', common + ['only_a_regular_file_of_the_scratch_is_handed', '--', '--nocapture', '--test-threads=1'])
if not result:
    result = run('candidate-callers', common + ['a_programs_call_is_its_callers', '--', '--nocapture', '--test-threads=1'])
if not result:
    result = run('candidate-clippy', ['cargo', 'clippy', '-p', 'shards-server', '--locked', '--all-targets', '--profile', 'test-release', '-j2'] + target_args + ['--', '-D', 'warnings'])
run('storage', ['du', '-sh', str(scope), str(source)])
sys.exit(result)
