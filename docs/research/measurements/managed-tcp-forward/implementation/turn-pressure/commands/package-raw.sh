python3 - <<'PY'
from pathlib import Path
import os,json,hashlib,subprocess,tarfile,gzip,io
home=Path.home();final=home/'relay-turn-pressure-2300-v5';packet=final/'packet';packet.mkdir(exist_ok=False);entries={};prep=[]
for version in [1,2,3,4,5]:
 root=home/('relay-turn-pressure-2300-v'+str(version));label='v'+str(version)
 for folder in ['record','cases']:
  if not (root/folder).exists():continue
  for p in sorted((root/folder).rglob('*')):
   if p.is_symlink():raise RuntimeError('unexpected raw symlink')
   if p.is_file():entries[label+'/'+p.relative_to(root).as_posix()]=p.read_bytes()
 result=json.loads((root/'record/result.json').read_text())
 prep.append({'version':version,'source_revision':result['source_revision'],'native_cases':len(result['cases']),'error':result['error'],'fd_before':result['fd_before'],'fd_after':result['fd_after'],'all_children_pidfd_exit_reaped_no_forced_cleanup':all(r['pidfd_exit_ready'] and r['reaped'] and not r['forced_owned_cleanup'] for r in result['phases']),'passed':result['passed']})
 for name in ['Cargo.toml','Cargo.lock']:
  p=root/'check'/name
  if p.exists():entries[label+'/projection/'+name]=p.read_bytes()
 for p in sorted((root/'target').glob('**/*.long-type-*.txt')) if (root/'target').exists() else []:
  entries[label+'/diagnostics/'+p.name]=p.read_bytes()
for version in [1,2]:
 root=home/('relay-turn-pressure-format-2300-v'+str(version))
 for p in sorted(root.iterdir()):
  if p.is_file():entries['formatter-v'+str(version)+'/'+p.name]=p.read_bytes()
source=final/'source'
for name in ['Cargo.lock','Cargo.toml','clippy.toml','rustfmt.toml','.cargo/config.toml','crates/testguest/src/interface_root.rs','crates/init/src/relay/turn_native.rs','docs/research/measurements/managed-tcp-forward/implementation/turn-pressure/native_case.py','docs/research/measurements/managed-tcp-forward/implementation/turn-pressure/source-identities.json']:
 entries['source-context/'+name]=(source/name).read_bytes()
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
result=json.loads((final/'record/result.json').read_text());assert result['passed']
binaries=[{'path':str(p),'bytes':p.stat().st_size,'sha256':digest(p)} for p in sorted((final/'artifacts').iterdir())]
env={'uname':list(os.uname()),'os_release':Path('/etc/os-release').read_text(),'rustc':subprocess.check_output(['rustup','run','1.98.1','rustc','-vV'],text=True),'cargo':subprocess.check_output(['rustup','run','1.98.1','cargo','-V'],text=True),'libc':subprocess.check_output(['ldd','--version'],text=True,stderr=subprocess.STDOUT),'binaries':binaries}
entries['ENVIRONMENT.json']=(json.dumps(env,indent=2)+'\n').encode()
summary={'source_revision':result['source_revision'],'preparation':prep,'binaries':binaries,'phases':result['phases'],'cases':result['cases'],'fd_before':result['fd_before'],'fd_after':result['fd_after'],'kernel_return':'Unknown','physical_kernel_caps':'Unverified','no_actual_vm':True,'passed':result['passed']}
entries['SUMMARY.json']=(json.dumps(summary,indent=2)+'\n').encode()
rows=[{'path':name,'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()} for name,b in sorted(entries.items())]
entries['MANIFEST.json']=(json.dumps({'files':rows},indent=2)+'\n').encode()
archive=packet/'raw-linux.tar.gz'
with archive.open('xb') as output:
 with gzip.GzipFile(fileobj=output,mode='wb',filename='',mtime=0) as gz:
  with tarfile.open(fileobj=gz,mode='w') as tar:
   for name,b in sorted(entries.items()):
    entry=tarfile.TarInfo(name);entry.size=len(b);entry.mode=0o644;entry.mtime=0;entry.uid=0;entry.gid=0;tar.addfile(entry,io.BytesIO(b))
compact=[]
for c in result['cases']:
 prefill=[x for x in c['observed'] if x['type']=='NativeTurnPrefill'];snapshots=[x for x in c['observed'] if x['type']=='NativeTurnSnapshot'];terminal=[x for x in c['observed'] if x['type']=='NativeTurnTerminal'];cleanup=[x for x in c['observed'] if x['type']=='NativeTurnCleanup']
 receipts=[{'role':x['role'],'bytes':x['bytes'],'sha256':hashlib.sha256(bytes.fromhex(x['hex'])).hexdigest()} for x in c['observed'] if x['type']=='NativeTurnBytes']
 compact.append({'target':c['target'],'result':c['result'],'prefill':prefill,'first_two_turns':snapshots[:2],'recovery_turns':len(snapshots)-2,'terminal':terminal,'cleanup':cleanup,'byte_receipts':receipts})
print(json.dumps({'archive':str(archive),'bytes':archive.stat().st_size,'sha256':digest(archive),'regular_members':len(entries),'original_files':len(rows),'uncompressed_bytes':sum(len(b) for b in entries.values()),'preparation':prep,'binaries':binaries,'native':compact}))
PY
