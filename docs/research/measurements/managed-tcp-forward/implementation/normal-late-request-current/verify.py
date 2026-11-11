#!/usr/bin/env python3
"""Verify frozen ordinary-case evidence; never build or start actors."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import tarfile

ROOT = Path(__file__).resolve().parent
ARCHIVES = [
    ('execution/raw-linux.tar.gz', 'manifest.json', 31242, '61957e3947b525575b8f676e734e9513a69185d707395f79be9f357795790963', 88),
    ('source-package-v1.tar.gz', 'package-manifest.json', 14184, 'd8a7c93c7d63b72c145044356060380ee29ae26cbd954830cb8dc3d535b721b3', 7),
]


def main():
    for relative, manifest_name, size, digest, payloads in ARCHIVES:
        path = ROOT / relative
        data = path.read_bytes()
        assert len(data) == size and hashlib.sha256(data).hexdigest() == digest
        with tarfile.open(path, 'r:gz') as retained:
            members = retained.getmembers()
            names = [member.name for member in members]
            assert len(names) == len(set(names)) == payloads + 1
            assert all(member.isreg() and not PurePosixPath(member.name).is_absolute()
                       and '..' not in PurePosixPath(member.name).parts for member in members)
            manifest = json.loads(retained.extractfile(manifest_name).read())
            expected = {entry['path']: entry for entry in manifest['files']}
            assert set(names) == set(expected) | {manifest_name}
            for member in members:
                if member.name == manifest_name:
                    continue
                raw = retained.extractfile(member).read()
                entry = expected[member.name]
                assert len(raw) == entry['bytes']
                assert hashlib.sha256(raw).hexdigest() == entry['sha256']
            if relative.startswith('execution/'):
                final = json.loads(retained.extractfile('record/FINAL.json').read())
                assert final['status'] == 'COMPLETE' and final['passed'] is True
                assert final['native_cases_started'] == 2 and final['runtime_retry'] is False
                assert final['mutation'] is False and final['source_unchanged'] is True
                assert final['controller_fd_before'] == final['controller_fd_after'] == 9
                for case in ('late-request', 'forward-server-first'):
                    stdout = retained.extractfile(f'cases/{case}/stdout').read().decode()
                    actual = [json.loads(line.split('managed-tcp-forward ', 1)[1])
                              for line in stdout.splitlines() if 'managed-tcp-forward ' in line]
                    assert actual == json.loads(retained.extractfile(f'cases/{case}/observed.json').read())
                    result = json.loads(retained.extractfile(f'cases/{case}/result.json').read())
                    assert result['status'] == 0 and result['passed'] is True
                    assert all(result['checks'].values()) and '1 passed; 0 failed;' in stdout
                ready = json.loads(retained.extractfile('record/READY.json').read())
                assert ready['native_cases_started'] == 0
                assert all('1 test, 0 benchmarks' in value for value in ready['listings'].values())
        print(f'{relative}: {payloads} payloads + manifest, all exact')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
