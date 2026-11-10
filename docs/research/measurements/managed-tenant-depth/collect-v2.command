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
records=base/'records'
archive=base/'raw-v2.tar.gz'
assert not archive.exists()
files=sorted([p for folder in ['live-d3-v1','live-single-v1'] for p in (records/folder).rglob('*') if not p.is_dir()]+[records/name for name in ['prepare-d3-inputs.command','live-d3.command','prepare-single-inputs.command','live-single.command','collect-v2.command']])
rows=[]
for p in files:
    assert not p.is_symlink() and stat.S_ISREG(p.stat().st_mode),p
    rel=p.relative_to(records).as_posix()
    assert not rel.startswith('/') and '..' not in Path(rel).parts
    data=p.read_bytes()
    rows.append({'path':rel,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
manifest={'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'canonical_base':'b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe','policy_sha256':'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3','files':rows,'original_count':len(rows),'full_population_kernel_independent_wait_return':'Unknown'}
encoded=(json.dumps(manifest,indent=2)+'\n').encode()
with archive.open('xb') as raw:
    with gzip.GzipFile(filename='',fileobj=raw,mode='wb',mtime=0) as zipped:
        with tarfile.open(fileobj=zipped,mode='w|') as t:
            for p,row in zip(files,rows): t.add(p,arcname='raw-v2/'+row['path'],recursive=False)
            info=tarfile.TarInfo('raw-v2/MANIFEST.json'); info.size=len(encoded); info.mode=0o600
            t.addfile(info,io.BytesIO(encoded))
receipt={'path':str(archive),'bytes':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'original_count':len(rows),'archive_regular_members':len(rows)+1,'source':manifest['source']}
(base/'raw-v2.receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
PYCOLLECT
