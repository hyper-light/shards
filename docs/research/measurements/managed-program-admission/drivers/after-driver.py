import pathlib, subprocess, json, os, hashlib, shutil, sys
repo=pathlib.Path.home()/'managed-capture-check'
out=pathlib.Path.home()/'root-managed-program-admission-after-v1'
out.mkdir(exist_ok=False)
def run(name,argv,timeout=900):
 (out/(name+'.command.json')).write_text(json.dumps(argv)+'\n')
 with (out/(name+'.log')).open('wb') as f:
  try: rc=subprocess.run(argv,cwd=repo,stdout=f,stderr=subprocess.STDOUT,env=os.environ.copy(),timeout=timeout).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);return rc
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
if run('fetch',['git','fetch','origin','p/managed-layer-production-v1'],120):sys.exit(1)
if run('checkout',['git','checkout','--detach','origin/p/managed-layer-production-v1'],120):sys.exit(1)
sha=subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()
(out/'identity.json').write_text(json.dumps({'source':sha,'disk_free':shutil.disk_usage(repo).free,'server_sha256':hashlib.sha256((repo/'crates/server/src/server.rs').read_bytes()).hexdigest(),'sandbox_sha256':hashlib.sha256((repo/'crates/server/src/sandbox.rs').read_bytes()).hexdigest()},indent=2)+'\n')
rc=run('server-tests',['cargo','test','-p','shards-server','--profile','test-release','-j','2','--','--nocapture'])
print((out/'server-tests.log').read_text()[-12000:],flush=True)
if rc:sys.exit(rc)
rc=run('server-clippy',['cargo','clippy','-p','shards-server','--all-targets','-j','2','--','-D','warnings'])
print((out/'server-clippy.log').read_text()[-6000:],flush=True)
sys.exit(rc)
