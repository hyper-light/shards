#!/usr/bin/env python3
"""Alternate two builds of the existing, unchanged deep-every regression test."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import platform
import re
import subprocess
import time


CASE = 'every_statements_nested_deep_answer_in_time'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', type=Path)
    parser.add_argument('after', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--before-source', required=True)
    parser.add_argument('--after-source', required=True)
    parser.add_argument('--rustc', required=True)
    parser.add_argument('--profile', default='test-release; opt-level=3; no LTO; codegen-units=16')
    parser.add_argument('--n', type=int, default=20)
    args = parser.parse_args()
    if args.n < 1:
        parser.error('--n must be positive')
    binaries = {'before': args.before.resolve(), 'after': args.after.resolve()}
    out = args.output.resolve()
    out.mkdir()  # Never overwrite a previous experiment.
    metadata = {
        'host': platform.node(), 'os': platform.platform(), 'machine': platform.machine(),
        'before_source': args.before_source, 'after_source': args.after_source,
        'rustc': args.rustc, 'profile': args.profile, 'case': CASE,
        'binary_sha256': {name: hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest()
                          for name, binary in binaries.items()},
        'n_per_build': args.n, 'unit': 'ms', 'clock': 'perf_counter_ns; entire test process',
    }
    (out / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')

    def run(name, tag):
        start = time.perf_counter_ns()
        result = subprocess.run([str(binaries[name]), CASE, '--exact', '--nocapture', '--test-threads=1'],
                                cwd=binaries[name].parent, capture_output=True)
        elapsed = (time.perf_counter_ns() - start) / 1e6
        (out / (tag + '.stdout.txt')).write_bytes(result.stdout)
        (out / (tag + '.stderr.txt')).write_bytes(result.stderr)
        if result.returncode or not re.search(rb'test result: ok\. 1 passed; 0 failed;', result.stdout):
            raise SystemExit(f'{tag} did not pass exactly one test; inspect raw output')
        return elapsed

    for name in binaries:
        run(name, 'warm-' + name)
    rows = []
    with (out / 'rows.jsonl').open('w') as file:
        for i in range(args.n):
            order = ['before', 'after'] if i % 2 == 0 else ['after', 'before']
            for name in order:
                row = {'iteration': i, 'build': name, 'elapsed_ms': run(name, f'{i:03}-{name}')}
                rows.append(row)
                file.write(json.dumps(row) + '\n')
                file.flush()
    summary = {}
    for name in binaries:
        values = sorted(row['elapsed_ms'] for row in rows if row['build'] == name)
        percentile = lambda p: values[max(0, math.ceil(len(values) * p) - 1)]
        summary[name] = {'n': len(values), 'p50': percentile(.5), 'p90': percentile(.9),
                         'p99': percentile(.99), 'max': values[-1]}
    (out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == '__main__':
    main()
