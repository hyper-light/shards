python3 - <<'PY'
from pathlib import Path
import hashlib,json,os,shutil,subprocess
h=Path.home();root=h/'managed-combined-seven-target-check';record=h/'root-managed-seven-d228-v1';record.mkdir(exist_ok=False)
assert shutil.disk_usage(h).free >=30*1024**3,'Mac below floor before build'
assert not subprocess.check_output(['git','-C',str(root),'diff','--name-only'],text=True).strip()
assert not subprocess.check_output(['git','-C',str(root),'diff','--cached','--name-only'],text=True).strip()
untracked=subprocess.check_output(['git','-C',str(root),'ls-files','--others','--exclude-standard'],text=True).splitlines()
allowed={'.DS_Store','crates/.DS_Store','docs/.DS_Store','docs/research/.DS_Store'}
assert set(untracked)<=allowed,untracked
(record/'untracked-preserved.json').write_text(json.dumps([{'path':p,'sha256':hashlib.sha256((root/p).read_bytes()).hexdigest(),'classification':'Finder metadata preserved, not source or build receipt'} for p in untracked],indent=2)+'\n')
with (record/'fetch.log').open('wb') as log:subprocess.run(['git','-C',str(root),'fetch','origin','refs/heads/main:refs/remotes/origin/main','refs/heads/p/managed-terminal-order-proof-v1:refs/remotes/origin/p/managed-terminal-order-proof-v1'],stdout=log,stderr=subprocess.STDOUT,check=True)
rules=subprocess.check_output(['git','-C',str(root),'show','origin/main:docs/development-rules.md']);assert hashlib.sha256(rules).hexdigest()=='d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242'
with (record/'checkout.log').open('wb') as log:subprocess.run(['git','-C',str(root),'switch','--detach','d2288c27ea29acbb9809fa2ff1f60176dac440b9'],stdout=log,stderr=subprocess.STDOUT,check=True)
(record/'source.txt').write_text('d2288c27ea29acbb9809fa2ff1f60176dac440b9\n')
(record/'policy.txt').write_text('ROOT coordinator current main policies rechecked hashd634; full7targets format GPU; no VM\n')
(record/'empty-docker').mkdir()
env=dict(os.environ,CARGO_TARGET_DIR=str(h/'.cache/managed-combined-seven-target-b8fe'),CARGO_BUILD_JOBS='4',DOCKER_CONFIG=str(record/'empty-docker'))
for key in ['SHARDS_INIT_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(key,None)
for name,args in [('rustc',['rustc','--version','--verbose']),('cargo',['cargo','--version'])]:(record/(name+'.txt')).write_text(subprocess.check_output(args,text=True))
cmd=['lockf','/tmp/g121-mac.lock','sh','scripts/lint'];(record/'command.json').write_text(json.dumps(cmd)+'\n')
with (record/'lint.log').open('wb') as log:result=subprocess.run(cmd,cwd=root,env=env,stdout=log,stderr=subprocess.STDOUT)
(record/'lint.exit').write_text(str(result.returncode)+'\n');(record/'free.post.json').write_text(json.dumps({'free_bytes':shutil.disk_usage(h).free})+'\n')
print('full-seven-b02e',result.returncode,flush=True)
if result.returncode:raise SystemExit(result.returncode)
assert shutil.disk_usage(h).free >=30*1024**3,'Mac below floor before native tests'
cmd=['lockf','/tmp/g121-mac.lock','cargo','test','--locked','-j4','-p','shards-remote','--lib','managed::tests','--','--test-threads=1']
(record/'native.command.json').write_text(json.dumps(cmd)+'\n')
with (record/'native.log').open('wb') as log:result=subprocess.run(cmd,cwd=root,env=env,stdout=log,stderr=subprocess.STDOUT)
(record/'native.exit').write_text(str(result.returncode)+'\n')
print('native-remote-b02e',result.returncode,flush=True)
raise SystemExit(result.returncode)

PY
