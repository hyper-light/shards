"""Download the exact repository-pinned upstream Buildx Windows arm64 client."""
import hashlib
import json
from pathlib import Path
import struct
import urllib.request

kit=Path('/Users/adalundhe/g121-a4-winarm')
out=kit/'native-buildx01'
out.mkdir(exist_ok=True)
assert not (out/'docker-buildx.exe').exists(), 'Do not overwrite an existing verified client'
def get(url):
    request=urllib.request.Request(url,headers={'User-Agent':'shards-step10-native-client-fixture'})
    with urllib.request.urlopen(request,timeout=120) as response:return response.read()
metadata=json.loads(get('https://api.github.com/repos/docker/buildx/releases/tags/v0.37.1'))
assert metadata['tag_name']=='v0.37.1'
assets={asset['name']:asset for asset in metadata['assets']}
name='buildx-v0.37.1.windows-arm64.exe'
assert name in assets,sorted(assets)
asset=assets[name]
checksum=assets['checksums.txt']
sums=get(checksum['browser_download_url'])
matching=[line.split()[0] for line in sums.decode().splitlines() if line.split()[-1].lstrip('*')==name]
assert len(matching)<=1
expected = matching[0] if matching else asset.get('digest','').removeprefix('sha256:')
assert len(expected)==64 and all(c in '0123456789abcdef' for c in expected)
contents=get(asset['browser_download_url'])
actual=hashlib.sha256(contents).hexdigest()
assert actual==expected
if asset.get('digest'):assert asset['digest']=='sha256:'+actual
assert len(contents)==asset['size']
offset=struct.unpack_from('<I',contents,0x3c)[0]
assert contents[:2]==b'MZ' and contents[offset:offset+4]==b'PE\0\0'
assert struct.unpack_from('<H',contents,offset+4)[0]==0xaa64
(out/'docker-buildx.exe').write_bytes(contents)
(out/'checksums.txt').write_bytes(sums)
record={'upstream':metadata['html_url'],'tag':metadata['tag_name'],'release_id':metadata['id'],
        'published_at':metadata['published_at'],'asset_id':asset['id'],'url':asset['browser_download_url'],
        'checksum_url':checksum['browser_download_url'],'sha256':actual,'bytes':len(contents),
        'integrity_source':'checksums.txt and asset digest' if matching else 'GitHub release asset digest; checksums.txt has no Windows asset entry',
        'pe_machine':'ARM64/0xaa64','deployed':False,'native_version_check':'pending'}
(out/'buildx.json').write_text(json.dumps(record,indent=2)+'\n')
print('Verified official Buildx0.37.1 Windows arm64: '+actual,flush=True)
