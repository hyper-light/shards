from pathlib import Path
import subprocess,json,os,hashlib,sys
repo=Path.home()/'managed-capture-check';out=Path.home()/'root-managed-borrowed-linux-3b196-v1';out.mkdir(exist_ok=False)
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()=='3b1967838b140466ff3367637541492a77be0459'
assert not subprocess.check_output(['git','diff','--name-only'],cwd=repo)
(out/'source.txt').write_text('3b1967838b140466ff3367637541492a77be0459\n')
def run(name,argv):
 (out/(name+'.command.json')).write_text(json.dumps(argv)+'\n')
 with (out/(name+'.log')).open('wb') as f:
  try:rc=subprocess.run(argv,cwd=repo,stdout=f,stderr=subprocess.STDOUT,timeout=900).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);print((out/(name+'.log')).read_text()[-7500:],flush=True);return rc
rc=run('api-native',['cargo','test','--locked','-j','2','-p','shards-api','--profile','test-release','--lib','--','--test-threads=1','--nocapture'])
if rc:sys.exit(rc)
rc=run('api-alloc-refusal',['cargo','test','--locked','-j','2','-p','shards-api','--profile','test-release','--lib','--','--ignored','--exact','client::borrowed_tests::shared_state_box_and_clone_oom_return_without_error_allocation','--nocapture'])
sys.exit(rc)
