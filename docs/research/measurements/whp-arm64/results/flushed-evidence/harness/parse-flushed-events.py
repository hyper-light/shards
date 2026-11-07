"""Materialize EVTX fields immediately; retain only System and OpenSSH records.

dissect.eventlog's cached templates reuse lazy substitutions across records. A
saved unmaterialized record is not a stable observation of its original fields.
Input is already-captured local event files, never a live filesystem or registry.
"""
import argparse
import datetime
import hashlib
import importlib.metadata
import json
from pathlib import Path

from dissect.eventlog.bxml import BxmlSub
from dissect.eventlog.evtx import Evtx


def freeze(value):
    if isinstance(value, BxmlSub):
        return freeze(value.get())
    if isinstance(value, dict):
        return {str(key): freeze(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [freeze(item) for item in value]
    if isinstance(value, (datetime.datetime, datetime.date)):
        return value.isoformat()
    if isinstance(value, bytes):
        return value.hex()
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    return str(value)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('capture', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    assert not args.output.exists(), 'Retain prior interpretations; do not overwrite'
    report = {'method': 'Materialize every BxmlSub at iteration time',
              'versions': {name: importlib.metadata.version(name) for name in
                           ['dissect.ntfs', 'dissect.cstruct', 'dissect.util', 'dissect.eventlog']},
              'files': {}}
    for name in ['System.evtx', 'OpenSSH%4Operational.evtx']:
        rows = []
        count = 0
        first = first_ref = None
        latest = ''
        path = args.capture / name
        with path.open('rb') as stream:
            for raw in Evtx(stream):
                row = freeze(raw)
                count += 1
                if first is None:
                    first, first_ref = row, raw
                latest = max(latest, str(row.get('TimeCreated_SystemTime', '')))
                if (name.startswith('OpenSSH') or
                    row.get('EventID') in [1, 12, 13, 26, 41, 43, 1001, 6005, 6006, 6008, 2004] or
                    'Hyper-V-Hypervisor' in str(row.get('Provider_Name', ''))):
                    rows.append(row)
        rows.sort(key=lambda row: str(row.get('TimeCreated_SystemTime', '')))
        report['files'][name] = {
            'capture_sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
            'native_records': count, 'latest_event': latest, 'selected': rows,
            'first_reference_changed_after_iteration': first != freeze(first_ref),
        }
        print(name, count, 'records; lazy-reference control', first != freeze(first_ref), flush=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
