python3 - <<'PY'
import hashlib,json,pathlib,tarfile,tomllib
base=pathlib.Path('/home/adalundhe/native-terminal-order-light-b02e-v1')
a=tomllib.loads((base/'source/Cargo.lock').read_text())['package'];b=tomllib.loads((base/'fixture-v2/Cargo.lock').read_text())['package']
def key(p):return (p['name'],p['version'],p.get('source','path'))
prior={key(p) for p in a};now={key(p) for p in b}
comparison={'added_package_identities':[list(p) for p in sorted(now-prior)],'removed_package_identity_count':len(prior-now),'note':'workspace narrowed to fixture; additions must be only terminal-order-light; source Cargo.lock retained'}
(base/'raw-v2/lock-comparison.json').write_text(json.dumps(comparison,indent=2)+'\n')
files=[]
for name in ('raw','raw-v2','fixture','fixture-v2'):
 for p in sorted((base/name).rglob('*')):
  if p.is_file():files.append(p.relative_to(base).as_posix())
files+=['light.py','light-v1.py','source/Cargo.toml','source/Cargo.lock','source/crates/shards/src/daemon/managed/terminal_order_tests.rs']
manifest={name:{'bytes':(base/name).stat().st_size,'sha256':hashlib.sha256((base/name).read_bytes()).hexdigest()} for name in files}
(base/'ARTIFACT-MANIFEST.json').write_text(json.dumps(manifest,indent=2)+'\n')
files+=['ARTIFACT-MANIFEST.json']
archive=base/'light-artifacts.tar.gz'
with tarfile.open(archive,'w:gz') as target:
 for name in files:target.add(base/name,arcname=name,recursive=False)
data=archive.read_bytes()
print(json.dumps({'archive':str(archive),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'members':len(files),'lock_comparison':comparison,'uname':(base/'raw/0.log').read_text().strip(),'rustc':(base/'raw/1.log').read_text().strip(),'cargo':(base/'raw/2.log').read_text().strip()}))
PY