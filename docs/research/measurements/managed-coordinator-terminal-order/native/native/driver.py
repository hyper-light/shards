from pathlib import Path
import hashlib,json,os,subprocess,shutil
h=Path.home();repo=h/'managed-capture-check';record=h/'root-managed-terminal-order-d228-v1';record.mkdir(exist_ok=False)
source='d2288c27ea29acbb9809fa2ff1f60176dac440b9';os.chdir(repo)
(record/'driver.py').write_text(Path(__file__).read_text());(record/'source.txt').write_text(source+'\n')
assert not subprocess.check_output(['git','status','--porcelain'],text=True).strip()
previous=json.loads((h/'root-managed-reconnect-diagnostic-v1/artifact.json').read_text())
frozen=h/'root-managed-reconnect-1ff90-artifacts-v1';frozen.mkdir(exist_ok=False)
p=Path(previous['path']);assert hashlib.sha256(p.read_bytes()).hexdigest()==previous['sha256'];target=frozen/'fixture';shutil.copy2(p,target)
assert hashlib.sha256(target.read_bytes()).hexdigest()==previous['sha256']
(frozen/'MANIFEST.json').write_text(json.dumps(previous|{'frozen_path':str(target),'source_revision':'1ff90db0043554eec18846f8f506f93f25e2ffec'},indent=2)+'\n')
with (record/'fetch.log').open('wb') as f:subprocess.run(['git','fetch','origin','refs/heads/main:refs/remotes/origin/main','refs/heads/p/managed-terminal-order-proof-v1:refs/remotes/origin/p/managed-terminal-order-proof-v1'],stdout=f,stderr=subprocess.STDOUT,check=True)
assert hashlib.sha256(subprocess.check_output(['git','show','origin/main:docs/development-rules.md'])).hexdigest()=='d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242'
with (record/'checkout.log').open('wb') as f:subprocess.run(['git','switch','--detach',source],stdout=f,stderr=subprocess.STDOUT,check=True)
assert subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()==source
settings={'CARGO_BUILD_JOBS':'4','CARGO_TARGET_DIR':str(repo/'target'),'DOCKER_CONFIG':str(record/'empty-docker')}
(record/'empty-docker').mkdir();env=os.environ.copy()
for key in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(key,None)
env.update(settings);(record/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
for name,args in [('rustc',['rustc','-Vv']),('cargo',['cargo','-V'])]:(record/(name+'.txt')).write_text(subprocess.check_output(args,text=True))
args=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--bin','shards','--no-run','--message-format=json'];(record/'compile.command.json').write_text(json.dumps(args)+'\n')
with (record/'compile.jsonl').open('wb') as out,(record/'compile.log').open('wb') as err:q=subprocess.run(args,env=env,stdout=out,stderr=err,timeout=900)
(record/'compile.exit').write_text(str(q.returncode)+'\n');print('compile',q.returncode,flush=True)
if q.returncode:raise SystemExit(q.returncode)
binaries=[v['executable'] for line in (record/'compile.jsonl').read_text().splitlines() if (v:=json.loads(line)).get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='shards' and v.get('profile',{}).get('test') and v.get('executable')]
assert binaries;binary=binaries[-1];digest=hashlib.sha256(Path(binary).read_bytes()).hexdigest()
(record/'artifact.json').write_text(json.dumps({'path':binary,'sha256':digest},indent=2)+'\n')
results=[]
for case in ('owner-finished-lock','owner-serving-lock','completion-lock','readonly-file','file-limit'):
 args=['python3','docs/research/measurements/managed-terminal-order/caller.py','--binary',binary,'--binary-sha256',digest,'--source-root',str(repo),'--source-revision',source,'--case',case,'--case-ms','300000','--timeout-seconds','900','--output',str(record/case)]
 if case=='file-limit':args+=['--prefix-bytes','1']
 (record/(case+'.command.json')).write_text(json.dumps(args)+'\n')
 with (record/(case+'.driver.log')).open('wb') as out:q=subprocess.run(args,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=1100)
 (record/(case+'.driver.exit')).write_text(str(q.returncode)+'\n')
 receipt=record/case/'receipt.json'
 results.append({'case':case,'caller_exit':q.returncode,'receipt':json.loads(receipt.read_text()) if receipt.exists() else None})
 (record/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(case,q.returncode,flush=True)
if any(v['caller_exit']!=0 for v in results):raise SystemExit(1)
(record/'driver.exit').write_text('0\n');print('full source daemon five actual native cases0',flush=True)

