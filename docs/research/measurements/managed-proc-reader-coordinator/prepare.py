#!/usr/bin/env python3
"""Coordinator-only exact-source extraction. Does not execute a campaign."""
import hashlib, json, pathlib, subprocess, sys
revision, preferred_revision, build_directory = sys.argv[1:]
build = pathlib.Path(build_directory)
build.mkdir(parents=True, exist_ok=False)
source = subprocess.check_output(['git', 'show', revision+':crates/api/src/taken.rs'])
start = source.index(b'    fn read_stat(')
end = source.index(b'\n    fn start_field(', start)
candidate = source[start:end].strip()+b'\n'
(build/'candidate.rs').write_bytes(candidate)
preferred = subprocess.check_output(['git', 'show', preferred_revision+
    ':docs/research/measurements/managed-proc-reader-coordinator/prototypes/src/lib.rs'])
(build/'preferred.rs').write_bytes(preferred)
here = pathlib.Path(__file__).resolve().parent
for name in ['probe.rs', 'usage.c']:
    (build/name).write_bytes((here/name).read_bytes())
(build/'source.json').write_text(json.dumps(dict(revision=revision,
    source_sha256=hashlib.sha256(source).hexdigest(),
    extracted_sha256=hashlib.sha256(candidate).hexdigest(),
    preferred_revision=preferred_revision,
    preferred_sha256=hashlib.sha256(preferred).hexdigest()), indent=2)+'\n')
