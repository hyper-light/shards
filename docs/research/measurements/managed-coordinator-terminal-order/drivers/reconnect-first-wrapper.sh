set -eu
p="$HOME/root-reconnect-54188-driver-v1.py"
test ! -e "$p"
cat > "$p" <<'PY'
from pathlib import Path
import hashlib,json,os,socket,subprocess
home=Path.home();repo=home/'managed-capture-check';record=home/'root-managed-reconnect-current-v1'
record.mkdir(exist_ok=False);os.chdir(repo)
(record/'driver.py').write_text(Path(__file__).read_text())
source='54188c747f2a64db7e1de564cd70df0bd16ca5af';(record/'source.txt').write_text(source+'\n')
assert not subprocess.check_output(['git','status','--porcelain'],text=True).strip()
fetch=['git','fetch','origin','refs/heads/main:refs/remotes/origin/main','refs/heads/p/managed-reconnect-proof-v1:refs/remotes/origin/p/managed-reconnect-proof-v1']
with (record/'fetch.log').open('wb') as f:subprocess.run(fetch,stdout=f,stderr=subprocess.STDOUT,check=True)
rules=subprocess.check_output(['git','show','origin/main:docs/development-rules.md'])
assert hashlib.sha256(rules).hexdigest()=='d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242'
with (record/'checkout.log').open('wb') as f:subprocess.run(['git','switch','--detach',source],stdout=f,stderr=subprocess.STDOUT,check=True)
assert subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()==source
env=os.environ.copy()
for key in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(key,None)
kernel=home/'managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy'
assert hashlib.sha256(kernel.read_bytes()).hexdigest()=='97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae'
(record/'empty-docker').mkdir()
settings={'CARGO_BUILD_JOBS':'4','CARGO_TARGET_DIR':str(repo/'target'),'DOCKER_CONFIG':str(record/'empty-docker'),'SHARDS_REQUIRE_VMS':'1','SHARDS_KERNEL':str(kernel),'SHARDS_TEST_KERNEL':str(kernel)}
for name in ('EXIT','STDIO','RECONNECT'):settings['SHARDS_TENANT_'+name+'_SOURCE_REVISION']=source
settings['SHARDS_OUTPUT_COST_SOURCE_REVISION']=source
env.update(settings);(record/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
for name,args in [('rustc',['rustc','-Vv']),('cargo',['cargo','-V'])]:(record/(name+'.txt')).write_text(subprocess.check_output(args,text=True))
args=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_output','--no-run','--message-format=json']
(record/'compile.command.json').write_text(json.dumps(args)+'\n')
with (record/'compile.jsonl').open('wb') as out,(record/'compile.log').open('wb') as err:p=subprocess.run(args,env=env,stdout=out,stderr=err,timeout=900)
(record/'compile.exit').write_text(str(p.returncode)+'\n');print('compile',p.returncode,flush=True)
if p.returncode:raise SystemExit(p.returncode)
binaries=[v['executable'] for line in (record/'compile.jsonl').read_text().splitlines() if (v:=json.loads(line)).get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert binaries;binary=binaries[-1]
(record/'artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n')
with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
folder=record/'reconnect';folder.mkdir()
extra={'SHARDS_TENANT_RECONNECT_PORT':str(port),'SHARDS_TENANT_RECONNECT_DEADLINE_MS':'300000','SHARDS_TENANT_RECONNECT_WORK_MS':'300000','SHARDS_TENANT_RECONNECT_CLEANUP_MS':'300000','SHARDS_TENANT_RECONNECT_RECORDS':str(folder/'records')}
(folder/'environment.json').write_text(json.dumps(extra,indent=2)+'\n')
test='tenant_exit::reconnect::fixture::observer_disconnect_replays_retained_record_without_losing_owner'
args=[binary,test,'--exact','--ignored','--nocapture','--test-threads=1']
(folder/'command.json').write_text(json.dumps(args)+'\n')
try:
 with (folder/'test.log').open('wb') as out:p=subprocess.run(args,env=env|extra,stdout=out,stderr=subprocess.STDOUT,timeout=900)
except subprocess.TimeoutExpired:
 (folder/'outer-timeout.json').write_text(json.dumps({'state':'TimedOut','cleanup':'Unknown; ROOT must reconcile before cache reuse','timeout_s':900})+'\n');raise
(folder/'test.exit').write_text(str(p.returncode)+'\n')
teardown=folder/'records/teardown.json'
result={'case':'reconnect','actual_exit':p.returncode,'teardown':json.loads(teardown.read_text()) if teardown.exists() else None}
(record/'result.json').write_text(json.dumps(result,indent=2)+'\n');print('reconnect',p.returncode,flush=True)
if p.returncode:raise SystemExit(p.returncode)
log=(folder/'test.log').read_text();assert '1 passed; 0 failed' in log
t=result['teardown'];assert t['protocol_complete'] and t['run_removed'] and t['owner_stop_observer_join']['complete'],t
args=[binary,'--nocapture','--test-threads=1'];(record/'ordinary.command.json').write_text(json.dumps(args)+'\n')
with (record/'ordinary.log').open('wb') as f:p=subprocess.run(args,env=env,stdout=f,stderr=subprocess.STDOUT,timeout=900)
(record/'ordinary.exit').write_text(str(p.returncode)+'\n');print('ordinary',p.returncode,flush=True)
if p.returncode:raise SystemExit(p.returncode)
assert '21 passed; 0 failed' in (record/'ordinary.log').read_text()
(record/'driver.exit').write_text('0\n');print('root reconnect+ordinary gate0',flush=True)

PY
python3 "$p"
