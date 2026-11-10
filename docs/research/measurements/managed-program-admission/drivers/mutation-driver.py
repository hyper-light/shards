from pathlib import Path
import subprocess, json, os, hashlib, sys
repo=Path.home()/'managed-capture-check';out=Path.home()/'root-managed-program-admission-mutation-v1';out.mkdir(exist_ok=False)
def run(name,args,seconds=900):
 (out/(name+'.command.json')).write_text(json.dumps(args)+'\n')
 with (out/(name+'.log')).open('wb') as f:
  try:rc=subprocess.run(args,cwd=repo,stdout=f,stderr=subprocess.STDOUT,timeout=seconds).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);return rc
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
assert run('fetch',['git','fetch','origin','p/managed-layer-production-v1'],120)==0
assert run('checkout',['git','checkout','--detach','3b1967838b140466ff3367637541492a77be0459'],120)==0
p=repo/'crates/server/src/server.rs'; original=p.read_bytes();(out/'original-server.sha256').write_text(hashlib.sha256(original).hexdigest()+'\n')
needle=b'let id = self\n            .next\n            .checked_add(1)\n            .ok_or(ProgramAdmission::IdentityExhausted)?;'
assert original.count(needle)==1
mutant=original.replace(needle,b'let id = self.next.wrapping_add(1);')
(out/'mutant-server.sha256').write_text(hashlib.sha256(mutant).hexdigest()+'\n')
try:
 p.write_bytes(mutant)
 rc=run('identity-guard-removed',['cargo','test','-p','shards-server','--profile','test-release','-j','2','--','--exact','server::tests::an_exhausted_program_identity_cannot_release_another_worker','--nocapture'])
 print((out/'identity-guard-removed.log').read_text()[-4500:],flush=True)
 assert rc==101,'mutation did not fail at expected test'
 assert 'worker_released=true' in (out/'identity-guard-removed.log').read_text(),'no independent actual native release receipt'
finally:
 p.write_bytes(original)
 assert p.read_bytes()==original
 assert not subprocess.check_output(['git','diff','--name-only'],cwd=repo)
 (out/'restored-server.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n')
rc=run('restored-server',['cargo','test','-p','shards-server','--profile','test-release','-j','2','--','--nocapture'])
print((out/'restored-server.log').read_text()[-6500:],flush=True)
sys.exit(rc)
