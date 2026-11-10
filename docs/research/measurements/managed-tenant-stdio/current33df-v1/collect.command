set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-current-33df-01a1c5d2-v1"
cd "$campaign"
exec 9> .campaign.lock
flock -n 9
python3 - <<'PY'
from pathlib import Path
import gzip,hashlib,json,stat,tarfile
records=Path('records');out=Path('packaged-raw-v1');out.mkdir()
files=[];remote_only=[]
for p in sorted(records.rglob('*')):
    mode=p.lstat().st_mode
    assert not stat.S_ISLNK(mode),str(p)
    if stat.S_ISDIR(mode): continue
    assert stat.S_ISREG(mode),str(p)
    b=p.read_bytes();rel=p.relative_to(records)
    entry={'original':rel.as_posix(),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()}
    if rel.as_posix()=='compile.jsonl':
        remote_only.append(entry);continue
    dest=out/rel
    if p.suffix=='.log' or rel.as_posix()=='commit.txt':
        dest=dest.with_name(dest.name+'.gz');stored=gzip.compress(b,mtime=0)
        assert gzip.decompress(stored)==b
    else: stored=b
    dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(stored)
    entry.update({'stored':dest.relative_to(out).as_posix(),'stored_bytes':len(stored),'stored_sha256':hashlib.sha256(stored).hexdigest()})
    files.append(entry)
manifest={'source':(records/'source.txt').read_text().strip(),'files':files,'remote_only':remote_only,'binary_and_source_preservation':'private source/target/cache/frozen unchanged; full CargoJSONL retained remotely','unknown':'full population/kernel/parent resource return unverified'}
(out/'STORAGE.json').write_text(json.dumps(manifest,indent=2)+'\n')
archive=Path('raw-v1.tar.gz')
with tarfile.open(archive,'w:gz') as t:
    for p in sorted(out.rglob('*')):
        if p.is_file(): t.add(p,arcname=p.relative_to(out),recursive=False)
b=archive.read_bytes()
receipt={'archive':str(archive.resolve()),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest(),'files':len(files)+1,'chunks':(len(b)+11999)//12000,'chunk_bytes':12000}
Path('raw-v1-transfer.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
PY
