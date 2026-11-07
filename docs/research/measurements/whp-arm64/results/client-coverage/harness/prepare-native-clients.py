"""Fetch pinned upstream clients on the Mac; do not mutate the live Windows fixture."""
import datetime
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import urllib.request

kit = Path('/Users/adalundhe/g121-a4-winarm')
out = kit / 'native-clients01'
out.mkdir(exist_ok=False)

def get(url):
    request = urllib.request.Request(url, headers={'User-Agent': 'shards-step10-native-client-fixture'})
    with urllib.request.urlopen(request, timeout=120) as response:
        return response.read()

tag = 'v5.6.0'
api = f'https://api.github.com/repos/docker/compose/releases/tags/{tag}'
metadata = json.loads(get(api))
assert metadata['tag_name'] == tag
assets = {asset['name']: asset for asset in metadata['assets']}
name = 'docker-compose-windows-aarch64.exe'
assert name in assets, sorted(assets)
asset = assets[name]
checksum = assets[name + '.sha256']
checksum_bytes = get(checksum['browser_download_url'])
expected = checksum_bytes.decode().split()[0].lower()
assert len(expected) == 64 and all(c in '0123456789abcdef' for c in expected)
contents = get(asset['browser_download_url'])
actual = hashlib.sha256(contents).hexdigest()
assert actual == expected
if asset.get('digest'):
    assert asset['digest'] == 'sha256:' + actual
assert len(contents) == asset['size']
offset = struct.unpack_from('<I', contents, 0x3c)[0]
assert contents[:2] == b'MZ' and contents[offset:offset+4] == b'PE\0\0'
assert struct.unpack_from('<H', contents, offset+4)[0] == 0xaa64, 'Not an arm64 PE'
(out / 'docker-compose.exe').write_bytes(contents)
(out / (name + '.sha256')).write_bytes(checksum_bytes)
record = {
    'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'compose': {
        'upstream': metadata['html_url'], 'tag': tag, 'release_id': metadata['id'],
        'published_at': metadata['published_at'], 'asset_id': asset['id'],
        'url': asset['browser_download_url'], 'sha256': actual, 'bytes': len(contents),
        'checksum_url': checksum['browser_download_url'], 'pe_machine': 'ARM64/0xaa64',
        'native_version_check': 'pending; current full suite unchanged',
    },
    'docker_cli': {
        'upstream': 'https://github.com/docker/cli', 'tag': 'v29.8.1',
        'commit': '4a63305d74332de5ceba7fcbccbc3cbb7412f5ba',
        'rationale': 'same exact upstream commit pinned by scripts/docker-cli/generate',
        'state': 'source download pending',
    },
    'deployed': False,
    'purpose': 'missing optional clients must not silently skip feature qualification',
}
(out / 'clients.json').write_text(json.dumps(record, indent=2) + '\n')
print('Verified official Compose5.6.0 Windows arm64: ' + actual, flush=True)
source = kit / 'docker-cli-29.8.1-source'
assert not source.exists(), 'Existing source must be inspected, not replaced'
subprocess.run(['git', 'clone', '--quiet', '--depth', '1', '--branch', 'v29.8.1',
                'https://github.com/docker/cli', str(source)], check=True)
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
assert head == record['docker_cli']['commit'], head
assert not subprocess.check_output(['git', 'status', '--porcelain'], cwd=source, text=True).strip()
record['docker_cli']['state'] = 'exact clean source ready; inspect upstream build script next'
(out / 'clients.json').write_text(json.dumps(record, indent=2) + '\n')
print('Verified pinned Docker CLI source ' + head + '; no Windows deployment', flush=True)
