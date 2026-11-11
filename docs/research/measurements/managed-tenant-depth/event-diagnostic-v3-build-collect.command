set -eu
campaign="$HOME/managed-tenant-event-diag-01a1c5d2-v2"
cd "$campaign/source"
exec 9>"$campaign/.light.lock"
flock -n 9
python3 - <<'PY'
from pathlib import Path
import json,hashlib,subprocess,tarfile,io
base=Path('..').resolve();r=base/'records';build=r/'build-v1'
assert (build/'compile.exit').read_text().strip()=='0'
assert (build/'testguest.exit').read_text().strip()=='0'
actual=json.loads((build/'artifacts.json').read_text())
assert actual['source']=='249dc4cf047586bcb040ae7895bc95769cde5b57'
assert not actual['all_six_product_native_bytes_match']
assert len(actual['artifacts'])==7
freeze=base/'frozen/249dc4cf0-v1'
for e in actual['artifacts']:
 for p in [Path(e['path']),freeze/Path(e['path']).name]:
  b=p.read_bytes();assert len(b)==e['bytes'] and hashlib.sha256(b).hexdigest()==e['sha256'],str(p)
assert not list(r.rglob('actor-started.once'))
assert not (r/'expiry-diagnostic-v1').exists()
source=json.loads((r/'source-readback.json').read_text())
for p,d in source['files'].items():assert hashlib.sha256(Path(p).read_bytes()).hexdigest()==d,p
assert subprocess.check_output(['git','rev-parse','HEAD']).decode().strip()==actual['source']
assert subprocess.check_output(['git','status','--porcelain'])==b''
receipt={'source':actual['source'],'source_hashes':source['files'],'host_compile_exit':0,'native_testguest_compile_exit':0,'host_strict_clippy_exit':json.loads((r/'result.json').read_text())['host_clippy_exit'],'frozen_all_seven_verified':True,'product_matches':sum(c['equal'] for c in actual['original_product_comparison']),'product_total':6,'vm_executions':0,'runtime_marker_present':False,'live_prepare_path_exists':False,'source_post_status':'','cause_of_binary_difference':'Unknown; no behavioral equivalence or product defect inferred','rustc':(build/'rustc.txt').read_text(),'cargo':(build/'cargo.txt').read_text()}
with (r/'r5-preservation-readback.json').open('x') as out:json.dump(receipt,out,indent=2);out.write('\n')
files=[('records/'+str(p.relative_to(r)),p) for p in sorted(r.rglob('*')) if p.is_file()]
files.append(('frozen/249dc4cf0-v1/manifest.json',freeze/'manifest.json'))
assert all(not p.is_symlink() for _,p in files)
manifest={'source':actual['source'],'vm_executions':0,'files':[{'path':name,'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for name,p in files]}
archive=base/'raw-diagnostic-build-r5-v1.tar.gz'
with archive.open('xb') as out,tarfile.open(fileobj=out,mode='w:gz') as t:
 for name,p in files:t.add(p,arcname=name,recursive=False)
 b=(json.dumps(manifest,indent=2)+'\n').encode();m=tarfile.TarInfo('MANIFEST.json');m.size=len(b);m.mode=0o600;t.addfile(m,io.BytesIO(b))
print(json.dumps({'archive':str(archive),'bytes':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'original_files':len(files),'receipt':receipt,'artifact_comparison':actual},indent=2))
PY
