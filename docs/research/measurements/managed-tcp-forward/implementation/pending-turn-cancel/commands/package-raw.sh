python3 - <<'PY'
from pathlib import Path
import os,json,hashlib,subprocess,tarfile,gzip,io
home=Path.home();root=home/'relay-turn-pending-cancel-0342-v1';packet=root/'packet';packet.mkdir(exist_ok=False);entries={}
for folder in ['record','cases']:
 for p in sorted((root/folder).rglob('*')):
  if p.is_symlink():raise RuntimeError('unexpected raw symlink')
  if p.is_file():entries[p.relative_to(root).as_posix()]=p.read_bytes()
formatter=home/'relay-turn-pending-cancel-0342-format-v1'
for p in sorted(formatter.iterdir()):
 if p.is_file():entries['formatter/'+p.name]=p.read_bytes()
source=root/'source'
for name in ['Cargo.toml','Cargo.lock','clippy.toml','rustfmt.toml','.cargo/config.toml','crates/testguest/src/interface_root.rs','crates/init/src/relay/turn_native.rs','docs/research/measurements/managed-tcp-forward/implementation/pending-turn-cancel/native_case.py','docs/research/measurements/managed-tcp-forward/implementation/pending-turn-cancel/source-identities.json']:
 entries['source-context/'+name]=(source/name).read_bytes()
for name in ['Cargo.toml','Cargo.lock']:entries['projection/'+name]=(root/'check'/name).read_bytes()
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
result=json.loads((root/'record/result.json').read_text());assert result['passed']
binaries=[{'path':str(p),'bytes':p.stat().st_size,'sha256':digest(p)} for p in sorted((root/'artifacts').iterdir())]
env={'uname':list(os.uname()),'os_release':Path('/etc/os-release').read_text(),'rustc':subprocess.check_output(['rustup','run','1.98.1','rustc','-vV'],text=True),'cargo':subprocess.check_output(['rustup','run','1.98.1','cargo','-V'],text=True),'libc':subprocess.check_output(['ldd','--version'],text=True,stderr=subprocess.STDOUT),'binaries':binaries}
entries['ENVIRONMENT.json']=(json.dumps(env,indent=2)+'\n').encode()
summary={'source_revision':result['source_revision'],'base':'0342b85d9c808e3ced041a6c3ef0e9bf70438272','binaries':binaries,'phases':result['phases'],'cases':result['cases'],'fd_before':result['fd_before'],'fd_after':result['fd_after'],'no_actual_vm':True,'mutation_run':False,'kernel_return':'Unknown','physical_kernel_caps':'Unverified','passed':result['passed']}
entries['SUMMARY.json']=(json.dumps(summary,indent=2)+'\n').encode()
rows=[{'path':name,'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()} for name,b in sorted(entries.items())];entries['MANIFEST.json']=(json.dumps({'files':rows},indent=2)+'\n').encode()
archive=packet/'raw-linux.tar.gz'
with archive.open('xb') as output:
 with gzip.GzipFile(fileobj=output,mode='wb',filename='',mtime=0) as gz:
  with tarfile.open(fileobj=gz,mode='w') as tar:
   for name,b in sorted(entries.items()):
    entry=tarfile.TarInfo(name);entry.size=len(b);entry.mode=0o644;entry.uid=0;entry.gid=0;entry.mtime=0;tar.addfile(entry,io.BytesIO(b))
native=[]
for c in result['cases']:
 kept=[r for r in c['observed'] if r['type'] in ['NativeTurnPrefill','NativeTurnCancelSignal','NativeTurnCancelTerminal','NativeTurnTerminal','NativeTurnCleanup'] or (r['type']=='NativeTurnSnapshot' and r['stage'] in ['SourceReceived','BlockedSend','PendingCancel'])]
 receipts=[{'role':r['role'],'bytes':r['bytes'],'sha256':hashlib.sha256(bytes.fromhex(r['hex'])).hexdigest()} for r in c['observed'] if r['type']=='NativeTurnBytes']
 native.append({'target':c['target'],'case':c['case'],'result':c['result'],'witnesses':kept,'byte_receipts':receipts})
print(json.dumps({'archive':str(archive),'bytes':archive.stat().st_size,'sha256':digest(archive),'regular_members':len(entries),'original_files':len(rows),'uncompressed_bytes':sum(len(b) for b in entries.values()),'binaries':binaries,'all_phase_pidfd_exit_reaped_no_forced_cleanup':all(r['pidfd_exit_ready'] and r['reaped'] and not r['forced_owned_cleanup'] for r in result['phases']),'native':native}))
PY
