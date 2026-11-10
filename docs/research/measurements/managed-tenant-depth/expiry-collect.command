set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "${SHARDS_TENANT_DEPTH_REVISION:?exact source required}"
test -z "$(git status --porcelain)"
python3 - <<'PYCOLLECT'
from pathlib import Path
import hashlib,json,os,tarfile,stat,io,gzip
base=Path.home()/'managed-tenant-depth-a920-01a1c5d2-v1'
records=base/'records/expiry-v1'
archive=base/'raw-expiry-v1.tar.gz'
assert not archive.exists()
files=sorted(p for p in records.rglob('*') if not p.is_dir())
rows=[]
for p in files:
    assert not p.is_symlink() and stat.S_ISREG(p.stat().st_mode),p
    rel=p.relative_to(records).as_posix()
    assert not rel.startswith('/') and '..' not in Path(rel).parts
    data=p.read_bytes()
    rows.append({'path':rel,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
manifest={'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'fixture_code_base':'cba6d1c32c5d27ef34b5cd9b49f95834253d6448','canonical_policy':'0342b85d9c808e3ced041a6c3ef0e9bf70438272','policy_sha256':'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3','files':rows,'original_count':len(rows),'full_population_kernel_independent_wait_return':'Unknown'}
encoded=(json.dumps(manifest,indent=2)+'\n').encode()
with archive.open('xb') as raw:
    with gzip.GzipFile(filename='',fileobj=raw,mode='wb',mtime=0) as zipped:
        with tarfile.open(fileobj=zipped,mode='w|') as t:
            for p,row in zip(files,rows): t.add(p,arcname='raw-expiry-v1/'+row['path'],recursive=False)
            info=tarfile.TarInfo('raw-expiry-v1/MANIFEST.json'); info.size=len(encoded); info.mode=0o600
            t.addfile(info,io.BytesIO(encoded))
receipt={'path':str(archive),'bytes':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'original_count':len(rows),'archive_regular_members':len(rows)+1,'source':manifest['source']}
(base/'raw-expiry-v1.receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
PYCOLLECT
