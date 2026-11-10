set -eu
p="$HOME/root-reconnect-54188-repeat-driver-v1.py"
test ! -e "$p"
cat > "$p" <<'PY'
from pathlib import Path
import json,hashlib,os,subprocess,socket,shutil
h=Path.home();base=h/'root-managed-reconnect-current-v1';repo=h/'managed-capture-check';record=h/'root-managed-reconnect-current-repeat-v1';record.mkdir(exist_ok=False)
source='54188c747f2a64db7e1de564cd70df0bd16ca5af'
assert subprocess.check_output(['git','-C',str(repo),'rev-parse','HEAD'],text=True).strip()==source
assert not subprocess.check_output(['git','-C',str(repo),'status','--porcelain'],text=True).strip()
(record/'driver.py').write_text(Path(__file__).read_text());(record/'source.txt').write_text(source+'\n')
a=json.loads((base/'artifact.json').read_text());assert hashlib.sha256(Path(a['path']).read_bytes()).hexdigest()==a['sha256']
frozen=h/'root-managed-reconnect-54188-artifacts-v1';frozen.mkdir(exist_ok=False)
roles=json.loads((base/'reconnect/records/artifacts.json').read_text());manifest=[]
for role in ('fixture','cli','init','testguest'):
 item=roles[role];original=Path(item['path']);expected=item['sha256'].removeprefix('sha256:');p=original
 if not p.exists() and role=='testguest':p=h/'root-managed-terminal-before-26ba-artifacts-v1/testguest'
 assert p.exists() and hashlib.sha256(p.read_bytes()).hexdigest()==expected,(role,str(p))
 target=frozen/role;shutil.copy2(p,target);assert hashlib.sha256(target.read_bytes()).hexdigest()==expected
 manifest.append({'role':role,'recorded_original':str(original),'freeze_source':str(p),'path':str(target),'sha256':expected,'content_equivalence_not_inode':p!=original})
(frozen/'MANIFEST.json').write_text(json.dumps(manifest,indent=2)+'\n');(record/'frozen-artifacts.json').write_text(json.dumps(manifest,indent=2)+'\n')
env=os.environ.copy()
for key in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(key,None)
settings=json.loads((base/'environment.json').read_text());(record/'empty-docker').mkdir();settings['DOCKER_CONFIG']=str(record/'empty-docker');env.update(settings)
extra=json.loads((base/'reconnect/environment.json').read_text())
with socket.socket() as s:s.bind(('127.0.0.1',0));extra['SHARDS_TENANT_RECONNECT_PORT']=str(s.getsockname()[1])
extra['SHARDS_TENANT_RECONNECT_RECORDS']=str(record/'records')
(record/'environment.json').write_text(json.dumps(settings|extra,indent=2)+'\n')
args=json.loads((base/'reconnect/command.json').read_text());(record/'command.json').write_text(json.dumps(args)+'\n')
with (record/'test.log').open('wb') as f:p=subprocess.run(args,cwd=repo,env=env|extra,stdout=f,stderr=subprocess.STDOUT,timeout=900)
(record/'test.exit').write_text(str(p.returncode)+'\n')
t=record/'records/teardown.json';result={'actual_exit':p.returncode,'teardown':json.loads(t.read_text()) if t.exists() else None}
(record/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result),flush=True)
raise SystemExit(p.returncode)

PY
python3 "$p"
