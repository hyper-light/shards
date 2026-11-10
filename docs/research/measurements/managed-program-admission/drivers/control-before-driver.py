from pathlib import Path
import subprocess,json,sys
repo=Path.home()/'managed-capture-check';out=Path.home()/'root-managed-program-control-before-v1';out.mkdir(exist_ok=False)
def run(name,args):
 (out/(name+'.command.json')).write_text(json.dumps(args)+'\n')
 with (out/(name+'.log')).open('wb') as f:
  try:rc=subprocess.run(args,cwd=repo,stdout=f,stderr=subprocess.STDOUT,timeout=900).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);return rc
assert not subprocess.check_output(['git','diff','--name-only'],cwd=repo)
assert run('fetch',['git','fetch','origin','p/managed-layer-production-v1'])==0
assert run('checkout',['git','checkout','--detach','origin/p/managed-layer-production-v1'])==0
(out/'source.txt').write_text(subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True))
rc=run('native-before',['cargo','test','--locked','-j','2','-p','shards-server','--profile','test-release','--','--exact','server::tests::an_unusable_helper_control_is_not_admitted','--nocapture'])
print((out/'native-before.log').read_text()[-4500:],flush=True)
sys.exit(rc)
