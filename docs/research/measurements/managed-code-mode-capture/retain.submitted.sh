set -eu
cd "$HOME/native-code-mode-capture-0219-v1"
python3 - <<'PY'
import pathlib,json,os,hashlib,tarfile
root=pathlib.Path.cwd(); evidence=root/'evidence'; native=evidence/'run-v1/native'
def fact(p):
 with p.open('rb') as stream: digest=hashlib.file_digest(stream,'sha256').hexdigest()
 return {'path':str(p.relative_to(root)),'bytes':p.stat().st_size,'sha256':digest}
found=[]
for p in pathlib.Path('/proc').iterdir():
 if not p.name.isdigit() or int(p.name) in (os.getpid(),os.getppid()): continue
 try:
  cwd=os.readlink(p/'cwd');exe=os.readlink(p/'exe');cmd=(p/'cmdline').read_bytes().replace(b'\0',b' ').decode(errors='replace')
  if cwd.startswith(str(root)) or exe.startswith(str(root)) or str(root) in cmd: found.append({'pid':int(p.name),'cwd':cwd,'exe':exe,'args':cmd[:2000]})
 except OSError: pass
(evidence/'post-case-host-processes.json').write_text(json.dumps({'matching_owned_scope_processes':found,'limit':'one kernel process snapshot by owned cwd/exe/argv, not all kernel resource return'},indent=2))
cachefiles=[fact(p) for p in sorted((evidence/'run-v1/runtime-cache').rglob('*')) if p.is_file()]
(evidence/'runtime-cache-files.json').write_text(json.dumps(cachefiles,indent=2))
for name in ['capture-cuts.json','cleanup.json','guest-processes.log','main-wait.log','remove.log','owner-stop.log','managed-output.before-remove']:
 p=native/name
 print('NATIVE',name,p.read_text() if p.exists() else 'MISSING: no success inferred')
for member in range(3): print('MEMBER_STDOUT',member,(native/f'member-{member}-1.bin').read_text())
print('HOST_PROCESSES',json.dumps(found)); print('RUNTIME_CACHE',json.dumps(cachefiles))
selected=[p for p in sorted(evidence.rglob('*')) if p.is_file() and 'runtime-cache' not in p.parts and p.name!='manifest.json']
manifest=[fact(p) for p in selected]
(evidence/'manifest.json').write_text(json.dumps(manifest,indent=2));selected.append(evidence/'manifest.json')
archive=root/'retained-code-mode-capture.tar.gz';assert not archive.exists()
with tarfile.open(archive,'w:gz') as tf:
 for p in selected:tf.add(p,arcname=str(p.relative_to(root)),recursive=False)
print('ARCHIVE',json.dumps(fact(archive)))
PY
du -sh target source cargo-home zig-global zig-local evidence
