python3 - <<'PY'
from pathlib import Path
import os,json,hashlib,subprocess,tarfile,gzip,io
root=Path.home()/'relay-tcp-extra-backend-f6a-v1';packet=root/'packet';packet.mkdir(exist_ok=False);entries={}
for folder in ['record','cases']:
 for p in sorted((root/folder).rglob('*')):
  if p.is_symlink():raise RuntimeError('unexpected raw symlink')
  if p.is_file():entries[p.relative_to(root).as_posix()]=p.read_bytes()
source=root/'source'
for name in ['Cargo.lock','clippy.toml','.cargo/config.toml','crates/testguest/src/interface_root.rs']:
 p=source/name
 if p.exists():entries['source-context/'+name]=p.read_bytes()
for name in ['Cargo.toml','Cargo.lock']:
 entries['projection/'+name]=(root/'check'/name).read_bytes()
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
result=json.loads((root/'record/result.json').read_text());assert result['passed']
binaries=[{'path':str(p),'bytes':p.stat().st_size,'sha256':digest(p)} for p in sorted((root/'artifacts').iterdir())]
env={'uname':list(os.uname()),'os_release':Path('/etc/os-release').read_text(),'rustc':subprocess.check_output(['rustup','run','1.98.1','rustc','-vV'],text=True),'cargo':subprocess.check_output(['rustup','run','1.98.1','cargo','-V'],text=True),'libc':subprocess.check_output(['ldd','--version'],text=True,stderr=subprocess.STDOUT),'binaries':binaries}
entries['ENVIRONMENT.json']=(json.dumps(env,indent=2)+'\n').encode()
summary={'source_revision':result['source_revision'],'positive_cases':sum(not r['mutant'] for r in result['cases']),'expected_actual_effect_mutants':sum(r['mutant'] and r['expected_outcome'] for r in result['cases']),'phases':len(result['phases']),'all_phase_pidfd_exit_reaped_no_forced_cleanup':all(r['pidfd_exit_ready'] and r['reaped'] and not r['forced_owned_cleanup'] for r in result['phases']),'fd_before':result['fd_before'],'fd_after':result['fd_after'],'restoration':result['restoration'],'binaries':binaries,'cases':result['cases'],'no_actual_vm':True,'kernel_return':'Unknown','physical_kernel_caps':'Unverified','passed':result['passed']}
entries['SUMMARY.json']=(json.dumps(summary,indent=2)+'\n').encode()
rows=[{'path':name,'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()} for name,b in sorted(entries.items())]
entries['MANIFEST.json']=(json.dumps({'files':rows},indent=2)+'\n').encode()
archive=packet/'raw-linux.tar.gz'
with archive.open('xb') as output:
 with gzip.GzipFile(fileobj=output,mode='wb',filename='',mtime=0) as gz:
  with tarfile.open(fileobj=gz,mode='w') as tar:
   for name,b in sorted(entries.items()):
    entry=tarfile.TarInfo(name);entry.size=len(b);entry.mode=0o644;entry.mtime=0;entry.uid=0;entry.gid=0;tar.addfile(entry,io.BytesIO(b))
print(json.dumps({'archive':str(archive),'bytes':archive.stat().st_size,'sha256':digest(archive),'regular_members':len(entries),'original_files':len(rows),'uncompressed_bytes':sum(len(b) for b in entries.values()),'binaries':binaries,'native_summary':[{'name':r['name'],'status':r['result']['status'],'fd_before':r['result']['fd_before'],'fd_after':r['result']['fd_after'],'expected_outcome':r['expected_outcome'],'extra':[(o['role'],o['extra_hex'],o['read_end']) for o in r['observed'] if o.get('type')=='Endpoint'],'remaining_ns':[o['original_remaining_ns'] for o in r['observed'] if o.get('type')=='NativeJoined' and 'original_remaining_ns' in o]} for r in result['cases']]}))
PY
