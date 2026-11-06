#!/usr/bin/env python3
"""Package a Mac cross-build for the same absolute paths under Windows' C: drive."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
from urllib.parse import unquote, urlparse
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('workspace', type=Path)
    parser.add_argument('metadata', type=Path, help='cargo xwin test --no-run --message-format=json output')
    parser.add_argument('output', type=Path)
    parser.add_argument('--guest', type=Path, required=True, help='Linux shards-testguest')
    parser.add_argument('--foreign-init', type=Path, required=True)
    args = parser.parse_args()
    if not args.guest.is_file() or not args.foreign_init.is_file():
        raise SystemExit('both Linux guest binaries must already be built')
    root = args.workspace.resolve()
    objects = []
    for line in args.metadata.read_text().splitlines():
        try:
            objects.append(json.loads(line))
        except ValueError:
            pass
    artifacts = [o for o in objects if o.get('reason') == 'compiler-artifact' and o.get('executable')]
    tests = [o for o in artifacts if o.get('profile', {}).get('test')]
    tests.sort(key=lambda o: (0 if o['target']['name'] == 'snapshot' else
                             1 if o['target']['name'] == 'boot' else 2,
                             o['package_id'], o['target']['name'], o['executable']))
    if not tests or any(Path(t['executable']).suffix != '.exe' for t in tests):
        raise SystemExit('metadata must contain the Windows test executables')
    manifest = []
    written = set()
    with zipfile.ZipFile(args.output, 'w', zipfile.ZIP_DEFLATED, compresslevel=3) as z:
        def add(path, name=None):
            path = Path(path)
            name = name or str(path).lstrip('/')
            if name in written:
                return
            written.add(name)
            # Match a Windows Git checkout without symlink privileges. Never follow a
            # tracked link outside the checkout while packaging fixtures.
            if path.is_symlink():
                z.writestr(name, os.readlink(path))
            elif path.is_file():
                z.write(path, name)

        tracked = subprocess.check_output(['git', 'ls-files', '-z'], cwd=root).split(b'\0')
        for name in tracked:
            if name:
                add(root / os.fsdecode(name))
        for o in artifacts:
            if not o.get('profile', {}).get('test'):
                add(o['executable'])
        for o in objects:
            if o.get('reason') == 'build-script-executed' and o['package_id'].startswith('path+'):
                out = Path(o['out_dir'])
                for f in out.iterdir():
                    if f.is_file():
                        add(f)
                    elif f.name == 'helpers':
                        for helper in f.iterdir():
                            add(helper)
        for t in tests:
            exe = Path(t['executable'])
            remote = 'shards-step10/tests/' + exe.name
            add(exe, remote)
            package = unquote(urlparse(t['package_id'].split('#')[0].removeprefix('path+')).path)
            with exe.open('rb') as f:
                digest = hashlib.file_digest(f, 'sha256').hexdigest()
            manifest.append({'package': t['package_id'], 'name': t['target']['name'],
                             'exe': 'C:\\' + remote.replace('/', '\\'),
                             'cwd': 'C:' + package.replace('/', '\\'), 'sha256': digest})
        add(args.guest, 'shards-step10/test-guests/guest/shards-testguest')
        add(args.foreign_init, 'shards-step10/test-guests/guest-foreign/shards-init')
        add(Path(__file__).with_name('run-tests.ps1'), 'shards-step10/run-tests.ps1')
        z.writestr('shards-step10/test-manifest.json', json.dumps(manifest, indent=2))
    with args.output.open('rb') as f:
        digest = hashlib.file_digest(f, 'sha256').hexdigest()
    args.output.with_suffix('.sha256').write_text(digest + '\n')
    print(f'{len(tests)} test executables; {args.output.stat().st_size} bytes; SHA256 {digest}')


if __name__ == '__main__':
    main()
