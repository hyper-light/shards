#!/usr/bin/env python3
"""Only parser/tree units and compile/clippy checks; never invokes a VM or fork actor."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', type=Path, required=True)
args = parser.parse_args()
if not os.environ.get('CARGO_TARGET_DIR'):
    parser.error('set a coordinator-approved private CARGO_TARGET_DIR')
args.evidence.mkdir(parents=True, exist_ok=False)
root = Path.cwd()
paths = ['crates/testguest/src/' + n + '.rs' for n in
         ['lib', 'main', 'workload', 'limits', 'managed_limits']]
paths.append('crates/shards/tests/managed_limits.rs')
identity = {}
for label, command in [('source', ['git', 'rev-parse', 'HEAD']),
                       ('status', ['git', 'status', '--short']),
                       ('compiler', ['rustc', '-Vv']), ('kernel', ['uname', '-a'])]:
    run = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, check=True)
    identity[label] = run.stdout
identity.update(cwd=str(root), target=os.environ['CARGO_TARGET_DIR'])
(args.evidence / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')
hashes = {name: hashlib.sha256(Path(name).read_bytes()).hexdigest() for name in paths}
(args.evidence / 'source-sha256.json').write_text(json.dumps(hashes, indent=2) + '\n')
# Sync can retain an older source mtime than a target artifact. Force this package's
# current source to be checked; never reuse a stale diagnostic executable as proof.
for name in paths:
    Path(name).touch()
env = dict(os.environ, SHARDS_HELPERS='skip')
commands = [
    ['cargo', 'test', '--locked', '-j4', '--profile', 'test-release', '-p',
     'shards-testguest', '--lib', 'limits::', '--', '--nocapture'],
    ['cargo', 'test', '--locked', '-j4', '--profile', 'test-release', '-p',
     'shards', '--test', 'managed_limits', 'process_observation_checks_threads_and_parentage',
     '--', '--exact', '--nocapture'],
    ['cargo', 'clippy', '--locked', '-j4', '--profile', 'test-release', '-p',
     'shards-testguest', '--bin', 'shards-testguest', '--', '-D', 'warnings'],
    ['cargo', 'clippy', '--locked', '-j4', '--profile', 'test-release', '-p',
     'shards', '--test', 'managed_limits', '--', '-D', 'warnings'],
]
failed = False
for index, command in enumerate(commands):
    prefix = args.evidence / str(index)
    prefix.with_suffix('.command.json').write_text(json.dumps(command) + '\n')
    print('starting', index, ' '.join(command), flush=True)
    with prefix.with_suffix('.log').open('w') as log:
        run = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
    prefix.with_suffix('.exit').write_text(str(run.returncode) + '\n')
    print('completed', index, run.returncode, flush=True)
    if run.returncode:
        print(prefix.with_suffix('.log').read_text()[-12000:], flush=True)
        failed = True
if hashes != {name: hashlib.sha256(Path(name).read_bytes()).hexdigest() for name in paths}:
    raise RuntimeError('fixture source changed during light checks')
sys.exit(1 if failed else 0)
