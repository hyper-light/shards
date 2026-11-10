#!/usr/bin/env python3
"""Run the existing pure source checks with this task's explicit one-CPU envelope."""
import hashlib
import json
import os
from pathlib import Path
import sys

original_path = Path('docs/research/measurements/managed-network-sock-addr/light.py')
original = original_path.read_text()
assert original.count("'-j4'") == 4
adapted = original.replace("'-j4'", "'-j1'")
# Bind only this check process and descendants to one actual permitted CPU.
allowed = sorted(os.sched_getaffinity(0))
assert allowed
os.sched_setaffinity(0, {allowed[0]})
proof = Path(sys.argv[1]).expanduser().resolve()
assert not proof.exists()
try:
    exec(compile(adapted, str(original_path), 'exec'), {'__name__': '__main__'})
finally:
    if proof.is_dir():
        (proof/'driver-original.py').write_text(original)
        (proof/'driver-one-cpu.py').write_text(adapted)
        (proof/'driver-scheduling.json').write_text(json.dumps({
            'original_path': str(original_path),
            'original_sha256': hashlib.sha256(original.encode()).hexdigest(),
            'adapted_sha256': hashlib.sha256(adapted.encode()).hexdigest(),
            'only_text_replacement': "'-j4' -> '-j1' (four command arguments)",
            'actual_prior_allowed_cpus': allowed,
            'actual_check_cpus': sorted(os.sched_getaffinity(0)),
            'envelope': {'cpus': 1, 'memory_gib': 1, 'disk_gib': 2},
        }, indent=2)+'\n')
