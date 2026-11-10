#!/usr/bin/env python3
"""Freeze one exact role artifact without changing the production pin or source."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import subprocess
import sys

label=sys.argv[1]
assert label in ['old','new']
out=Path(os.environ['MANAGED_PROCESS_BUILD_EVIDENCE'])
out.mkdir(parents=True,exist_ok=True)
assert not (out/(label+'-role')).exists(), 'use a new evidence prefix; preserve frozen binaries'
fixture=Path('docs/research/measurements/managed-performance/process-compat/role.rs')
entry=Path('crates/remote/tests/managed_process.rs')
# Owned test sources only: forbid a stale artifact after mtime-preserving sync.
fixture.touch()
entry.touch()
command=['cargo','test','--locked','-j4','--release','-p','shards-remote','--test','managed_process','--no-run','--message-format=json']
with (out/(label+'-build.jsonl')).open('wb') as stdout,(out/(label+'-build.stderr')).open('wb') as stderr:
    subprocess.run(command,stdout=stdout,stderr=stderr,check=True)
artifacts=[json.loads(line) for line in (out/(label+'-build.jsonl')).read_text().splitlines()]
artifacts=[a for a in artifacts if a.get('target',{}).get('name')=='managed_process' and a.get('executable')]
assert len(artifacts)==1 and not artifacts[0]['fresh']
binary=out/(label+'-role')
shutil.copy2(artifacts[0]['executable'],binary)
record=dict(source=subprocess.check_output(['git','rev-parse','HEAD']).decode().strip(),
            binary=str(binary),sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
            fixture_sha256=hashlib.sha256(fixture.read_bytes()).hexdigest(),
            manifest_sha256=hashlib.sha256(Path('crates/remote/Cargo.toml').read_bytes()).hexdigest(),command=command)
(out/(label+'-role.json')).write_text(json.dumps(record,indent=2))
print(json.dumps(record),flush=True)
