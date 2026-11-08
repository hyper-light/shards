"""Retain native progress while SSH is unavailable; never launch/stop a VM/test."""
import datetime
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

KIT = Path('/Users/adalundhe/g121-a4-winarm')
EVIDENCE = KIT / 'evidence/tfa-psci'
PHASE = KIT / 'flushed-results-watcher01.json'
PYTHON = KIT / 'readonly-ntfs-venv/bin/python'
CAPTURE = KIT / 'capture-flushed-results.py'
EXPECTED_SOURCE = hashlib.sha256(CAPTURE.read_bytes()).hexdigest()


def state(**values):
    values.update(utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  source_sha256=EXPECTED_SOURCE,
                  scope='read-only flushed native evidence; no deployment or native task action')
    PHASE.write_text(json.dumps(values, indent=2) + '\n')
    print(values, flush=True)


def main():
    with PHASE.open('x') as output:
        json.dump({'phase': 'starting read-only observation'}, output)
    sequence = 1
    while True:
        assert hashlib.sha256(CAPTURE.read_bytes()).hexdigest() == EXPECTED_SOURCE, 'Capture source changed; inspect before continuing'
        target = EVIDENCE / f'readonly-progress-{sequence:04}'
        assert not target.exists(), 'Preserve an existing capture; no overwrite'
        with (KIT / f'readonly-progress-{sequence:04}.log').open('x') as output:
            result = subprocess.run([str(PYTHON), str(CAPTURE), str(target)],
                                    stdout=output, stderr=subprocess.STDOUT)
        assert result.returncode == 0, 'Read-only capture failed; inspect export cleanup before continuing'
        capture = json.loads((target / 'capture.json').read_text())
        assert capture['export_read_only'] and capture['export_stopped']
        rows = [json.loads(line) for line in (target / 'results.jsonl').read_text(encoding='utf-8-sig').splitlines() if line.strip()]
        assert all(row['index'] == index and isinstance(row['exit'], int) for index, row in enumerate(rows))
        failed = sum(row['exit'] != 0 for row in rows)
        text = (target / 'full-real04-normal-controller.txt').read_bytes().decode('utf-16')
        marker = re.search(r'^COMPLETE binaries=142 manifest=142 failed=(\d+) filter=/ start=0\s*$', text, re.M)
        terminal = len(rows) == 142 and marker is not None and int(marker[1]) == failed and (target / 'full-real04-normal-child.exit').exists()
        state(phase='full142 flushed terminal; inspect native archive' if terminal else 'watching original full task; no deployment',
              capture=str(target), native_records=len(rows), failed_records=failed,
              last_heartbeat=[line for line in text.splitlines() if line.startswith('ALIVE ')][-1:],
              complete_flushed_capture=terminal,
              qualification='original failures, client skips and old source still prevent Step10 qualification')
        if terminal:
            return
        sequence += 1
        time.sleep(600)


if __name__ == '__main__':
    main()
