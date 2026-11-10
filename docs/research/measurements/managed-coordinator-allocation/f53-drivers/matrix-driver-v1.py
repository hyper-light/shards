from pathlib import Path
import hashlib,json,os,shutil,subprocess
h=Path.home();root=h/'managed-combined-seven-target-check';record=h/'root-managed-seven-f53f-v1';record.mkdir(exist_ok=False)
assert shutil.disk_usage(h).free >=30*1024**3,'Mac below floor before build'
assert not subprocess.check_output(['git','-C',str(root),'status','--porcelain'],text=True).strip()
with (record/'fetch.log').open('wb') as log:subprocess.run(['git','-C',str(root),'fetch','origin','refs/heads/main:refs/remotes/origin/main','refs/heads/p/managed-combined:refs/remotes/origin/p/managed-combined'],stdout=log,stderr=subprocess.STDOUT,check=True)
rules=subprocess.check_output(['git','-C',str(root),'show','origin/main:docs/development-rules.md']);assert hashlib.sha256(rules).hexdigest()=='d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242'
with (record/'checkout.log').open('wb') as log:subprocess.run(['git','-C',str(root),'switch','--detach','f53f7750eb6c74ce6f7de290f6977c08a401f774'],stdout=log,stderr=subprocess.STDOUT,check=True)
(record/'source.txt').write_text('f53f7750eb6c74ce6f7de290f6977c08a401f774\n')
(record/'policy.txt').write_text('ROOT coordinator current main policies rechecked hashd634; full7targets format GPU; no VM\n')
(record/'empty-docker').mkdir()
env=dict(os.environ,CARGO_TARGET_DIR=str(h/'.cache/managed-combined-seven-target-b8fe'),CARGO_BUILD_JOBS='4',DOCKER_CONFIG=str(record/'empty-docker'))
for key in ['SHARDS_INIT_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(key,None)
for name,args in [('rustc',['rustc','--version','--verbose']),('cargo',['cargo','--version'])]:(record/(name+'.txt')).write_text(subprocess.check_output(args,text=True))
cmd=['lockf','/tmp/g121-mac.lock','sh','scripts/lint'];(record/'command.json').write_text(json.dumps(cmd)+'\n')
with (record/'lint.log').open('wb') as log:result=subprocess.run(cmd,cwd=root,env=env,stdout=log,stderr=subprocess.STDOUT)
(record/'lint.exit').write_text(str(result.returncode)+'\n');(record/'free.post.json').write_text(json.dumps({'free_bytes':shutil.disk_usage(h).free})+'\n')
print('full-seven-f53f',result.returncode,flush=True)
raise SystemExit(result.returncode)
