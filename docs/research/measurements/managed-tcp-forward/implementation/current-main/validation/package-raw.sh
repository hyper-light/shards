python3 - <<'PY'
from pathlib import Path
import os,json,hashlib,subprocess,tarfile,gzip,io
home=Path.home();root=home/'relay-current-1be-evidence-v1';root.mkdir(exist_ok=False)
scopes={'failed-harness':home/'relay-current-1be-a920-light-v1','current-selection':home/'relay-current-1be-a920-light-v2','pressure-repeat':home/'relay-current-1be-gnu-pressure-repeat-v1'}
entries={}
for label,scope in scopes.items():
 for sub in (['record','cases'] if label!='pressure-repeat' else ['.']):
  base=scope/sub
  if not base.exists():continue
  for p in sorted(base.rglob('*')):
   if p.is_symlink():raise RuntimeError('unexpected symlink')
   if p.is_file():entries[label+'/'+p.relative_to(scope).as_posix()]=p.read_bytes()
source=scopes['current-selection']/'source'
manifest=json.loads((source/'docs/research/measurements/managed-tcp-forward/implementation/current-main/source-identities.json').read_text())
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
identities=[{'path':r['path'],'bytes':(source/r['path']).stat().st_size,'expected_sha256':r['sha256'],'actual_sha256':digest(source/r['path'])} for r in manifest['paths']]
assert all(r['expected_sha256']==r['actual_sha256'] for r in identities)
diff=subprocess.run(['git','diff','--exit-code'],cwd=source,capture_output=True);assert diff.returncode==0
state={'observed_after_all_cases':True,'source_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'selected_identities':identities,'tracked_diff_status':diff.returncode,'tracked_diff_sha256':hashlib.sha256(diff.stdout).hexdigest(),'git_status':subprocess.check_output(['git','status','--short'],cwd=source,text=True),'binaries':[{ 'path':str(p),'bytes':p.stat().st_size,'sha256':digest(p)} for p in sorted((scopes['current-selection']/'artifacts').iterdir())],'no_actual_vm':True,'kernel_return':'Unknown','physical_kernel_caps':'Unverified'}
entries['SOURCE_AFTER.json']=(json.dumps(state,indent=2)+'\n').encode()
env={'uname':list(os.uname()),'os_release':Path('/etc/os-release').read_text(),'rustc':subprocess.check_output(['rustup','run','1.98.1','rustc','-vV'],text=True),'cargo':subprocess.check_output(['rustup','run','1.98.1','cargo','-V'],text=True),'libc':subprocess.check_output(['ldd','--version'],text=True,stderr=subprocess.STDOUT)}
entries['ENVIRONMENT.json']=(json.dumps(env,indent=2)+'\n').encode()
for name in ['Cargo.lock','clippy.toml','.cargo/config.toml']:
 p=source/name
 if p.exists():entries['current-selection/context/'+name]=p.read_bytes()
for label in ['failed-harness','current-selection']:
 for name in ['Cargo.toml','Cargo.lock','lib.rs']:
  p=scopes[label]/'check'/name
  if p.exists():entries[label+'/projection/'+name]=p.read_bytes()
selected=json.loads((scopes['current-selection']/'record/result.json').read_text());repeat=json.loads((scopes['pressure-repeat']/'result.json').read_text())
compact={'source_revision':state['source_revision'],'selection_run_passed':selected['passed'],'selection_error':selected['error'],'selection_successful_native_cases':len(selected['native']),'phases':selected['phases'],'native':selected['native'],'selection_failed_case_result':json.loads((scopes['current-selection']/'cases/x86_64-unknown-linux-gnu-pressure-backend-fin/result.json').read_text()),'selection_failed_case_observed':[row for row in json.loads((scopes['current-selection']/'cases/x86_64-unknown-linux-gnu-pressure-backend-fin/observed.json').read_text()) if row.get('type')!='Endpoint'],'exact_input_repeat':repeat,'source_after':state,'kernel_return':'Unknown','physical_kernel_caps':'Unverified'}
(root/'summary.json').write_text(json.dumps(compact,indent=2)+'\n')
entries['SUMMARY.json']=(root/'summary.json').read_bytes()
manifest_rows=[{'path':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()} for name,data in sorted(entries.items())]
entries['MANIFEST.json']=(json.dumps({'files':manifest_rows},indent=2)+'\n').encode()
archive=root/'raw-linux.tar.gz'
with archive.open('wb') as output:
 with gzip.GzipFile(fileobj=output,mode='wb',filename='',mtime=0) as gz:
  with tarfile.open(fileobj=gz,mode='w') as tar:
   for name,data in sorted(entries.items()):
    item=tarfile.TarInfo(name);item.size=len(data);item.mode=0o644;item.mtime=0;item.uid=0;item.gid=0;tar.addfile(item,io.BytesIO(data))
print(json.dumps({'root':str(root),'archive':str(archive),'bytes':archive.stat().st_size,'sha256':digest(archive),'regular_members':len(entries),'original_files':len(manifest_rows),'uncompressed_bytes':sum(len(x) for x in entries.values()),'summary_bytes':(root/'summary.json').stat().st_size,'binary_identities':state['binaries']}))
PY
