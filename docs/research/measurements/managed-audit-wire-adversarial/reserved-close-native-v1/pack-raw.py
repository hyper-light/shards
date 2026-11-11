from pathlib import Path,PurePosixPath
import hashlib,io,json,os,tarfile,time
root=Path.home()/'managed-audit-wire-reserved-close-v1'
budget=json.loads((root/'budget.json').read_text())
assert time.time()<budget['deadline_epoch']
paths=[root/'budget.json',root/'corpus-verified.json']
for name in ['build-records','case-controller-v1','native-records-v1']:
 paths+=sorted((root/name).rglob('*'))
files=[]
for p in paths:
 if p.is_dir():continue
 assert p.is_file() and not p.is_symlink(),str(p)
 raw=p.read_bytes();rel=p.relative_to(root).as_posix()
 files.append({'path':rel,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()})
assert len({f['path'] for f in files})==len(files)
manifest={'schema':1,'source_revision':'82e7243104aaf15b70a373d49fcdf4293136e012','runner':'d1323898b255a3ed435eb83fa0f7b0cf','native_case_attempts':1,'native_case_retries':0,'files':files,'exclusions_preserved_on_runner':['source','target','cargo-home','artifacts/shards-vmm-native','input','original-corpus-records','transfer','native-tmp-v1','source-package.tar.gz','source-package-manifest.json','build-native.py']}
mb=(json.dumps(manifest,indent=2)+'\n').encode()
with (root/'raw-manifest-v1.json').open('xb') as f:f.write(mb)
archive=root/'raw-native-v1.tar.gz'
with tarfile.open(archive,'x:gz') as t:
 for item in files:
  p=root/item['path'];t.add(p,arcname=item['path'],recursive=False)
 info=tarfile.TarInfo('manifest.json');info.size=len(mb);info.mode=0o644;t.addfile(info,io.BytesIO(mb))
raw=archive.read_bytes()
with tarfile.open(fileobj=io.BytesIO(raw)) as t:
 members=t.getmembers();assert len(members)==len(files)+1
 assert len({m.name for m in members})==len(members)
 assert all(m.isfile() and str(PurePosixPath(m.name))==m.name and not PurePosixPath(m.name).is_absolute() and '..' not in PurePosixPath(m.name).parts and chr(92) not in m.name for m in members)
 assert t.extractfile('manifest.json').read()==mb
 for item in files:
  b=t.extractfile(item['path']).read();assert len(b)==item['bytes'] and hashlib.sha256(b).hexdigest()==item['sha256']
receipt={'status':'PackedAllRawVerified_NoRerun','archive':str(archive),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'manifest_sha256':hashlib.sha256(mb).hexdigest(),'payloads':len(files),'total_members':len(files)+1,'epoch':time.time(),'original_deadline_epoch':budget['deadline_epoch'],'all_unique_safe_regular_hashes_verified':True}
with (root/'raw-pack-receipt-v1.json').open('x') as f:json.dump(receipt,f,indent=2)
print(json.dumps(receipt),flush=True)
