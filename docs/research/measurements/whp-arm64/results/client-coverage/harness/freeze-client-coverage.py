"""Freeze client coverage without changing the installed full suite or its queues."""
import datetime
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import zipfile

kit=Path('/Users/adalundhe/g121-a4-winarm')
root=Path('/Users/adalundhe/Projects/shards/.claude/worktrees/g121-wa64')
assert (kit/'validate-native-client-coverage-status.txt').read_text().strip()=='0'
assert (kit/'parse-run-tests-candidate.exit').read_text().strip()=='0'
patch=(kit/'client-coverage-source.patch').read_bytes()
assert patch==(kit/'client-coverage-source-final.patch').read_bytes()
assert patch==subprocess.check_output(['git','diff','HEAD'],cwd=root)
objects=[]
for line in (kit/'build-client-coverage-full.jsonl').read_text().splitlines():
    try:objects.append(json.loads(line))
    except ValueError:pass
tests=[o for o in objects if o.get('reason')=='compiler-artifact' and o.get('executable') and o.get('profile',{}).get('test')]
assert len(tests)==142
by_name={Path(o['executable']).name:Path(o['executable']) for o in tests}
with zipfile.ZipFile(kit/'full-next-142-initpath-candidate.zip') as archived:
    manifest=json.loads(archived.read('shards-step10/test-manifest.json'))
changed=[]
for test in manifest:
    name=test['exe'].replace('\\','/').rsplit('/',1)[-1]
    actual=hashlib.file_digest(by_name[name].open('rb'),'sha256').hexdigest()
    if actual!=test['sha256']:changed.append((test['name'],name,actual))
assert len(changed)==1 and changed[0][0]=='buildx',changed
out=kit/'client-coverage-candidate01'
out.mkdir(exist_ok=False)
name,digest=changed[0][1:]
shutil.copyfile(by_name[name],out/name)
for test in manifest:
    if test['name']=='buildx':
        test['exe']='C:\\shards-step10-clients01\\tests\\'+name
        test['sha256']=digest
(out/'test-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
runner=root/'docs/research/measurements/whp-arm64/nested-host/run-tests.ps1'
shutil.copyfile(runner,out/'run-tests.ps1')
shutil.copyfile(kit/'client-coverage-source.patch',out/'compiled-source.patch')
guest=root/'target/aarch64-unknown-linux-musl/guest/shards-testguest'
guest_sha=hashlib.file_digest(guest.open('rb'),'sha256').hexdigest()
storm=json.loads((kit/'storm-report-candidate01/candidate.json').read_text())
assert guest_sha==storm['guest_candidate_sha256']
shutil.copyfile(guest,out/'shards-testguest')
clients=json.loads((kit/'native-clients01/clients.json').read_text())
clients['buildx']=json.loads((kit/'native-buildx01/buildx.json').read_text())
(out/'clients').mkdir()
for binary,metadata,path in [
    ('docker.exe',clients['docker_cli'],kit/'native-clients01/docker.exe'),
    ('docker-compose.exe',clients['compose'],kit/'native-clients01/docker-compose.exe'),
    ('docker-buildx.exe',clients['buildx'],kit/'native-buildx01/docker-buildx.exe'),
]:
    assert hashlib.file_digest(path.open('rb'),'sha256').hexdigest()==metadata['sha256']
    shutil.copyfile(path,out/'clients'/binary)
(out/'clients/clients.json').write_text(json.dumps(clients,indent=2)+'\n')
record={
    'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'source_parent':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),
    'source_patch_sha256':hashlib.sha256(patch).hexdigest(),
    'all_eight_lint_targets_exit':0,'fresh_guest_build_exit':0,'fresh_windows_build_exit':0,
    'native_powershell_parse_exit':0,'host_executables_unchanged':141,
    'changed_host_binary':{'name':'buildx','file':name,'sha256':digest},
    'guest_candidate_sha256':guest_sha,'guest_original_sha256':storm['guest_original_sha256'],
    'runner_sha256':hashlib.file_digest((out/'run-tests.ps1').open('rb'),'sha256').hexdigest(),
    'manifest_sha256':hashlib.file_digest((out/'test-manifest.json').open('rb'),'sha256').hexdigest(),
    'deployed':False,'native_result':'pending; current full and both diagnostics are unchanged',
    'scope':'client-required Compose/Engine and Windows Buildx coverage; not a full-suite pass',
}
(out/'candidate.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps(record),flush=True)
