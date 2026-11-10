#!/usr/bin/env python3
"""Retain exact API subset and all parser includes for combined-revision checks."""
from pathlib import Path
import hashlib
import json
import runpy
import shutil
import sys
import tarfile

repo, target = map(Path, sys.argv[1:])
sys.argv = ['prepare-light.py', str(repo), str(target)]
runpy.run_path(str(repo/'docs/research/measurements/managed-borrowed-nowait/prepare-light.py'), run_name='__main__')
for name in ['repro.rs','reader-tests.rs']:
    relative=Path('docs/research/measurements/managed-proc-main')/name
    dest=target/relative
    dest.parent.mkdir(parents=True,exist_ok=True)
    shutil.copyfile(repo/relative,dest)
manifest=target/'source-hashes.json'
manifest.write_text(json.dumps({str(p.relative_to(target)):hashlib.sha256(p.read_bytes()).hexdigest()
                               for p in sorted(target.rglob('*')) if p.is_file() and p!=manifest},indent=2)+'\n')
archive=target.with_suffix('.tar.gz')
with tarfile.open(archive,'w:gz') as tar:
    for path in sorted(target.rglob('*')):
        if path.is_file():tar.add(path,arcname=str(path.relative_to(target)))
print(json.dumps({'candidate':'51fa61cef6c2e0435e8549a6014c1024dc497fc0','archive':str(archive),'bytes':archive.stat().st_size,
                  'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'source_files':len(json.loads(manifest.read_text()))}))
