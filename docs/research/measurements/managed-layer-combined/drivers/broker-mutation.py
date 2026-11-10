from pathlib import Path
import subprocess,json,hashlib,sys
repo=Path.home()/'managed-capture-check';out=Path.home()/'root-managed-broker-mutation-0219-v1';out.mkdir(exist_ok=False)
def run(name,args):
 (out/(name+'.command.json')).write_text(json.dumps(args)+'\n')
 with (out/(name+'.log')).open('wb') as log:
  try:rc=subprocess.run(args,cwd=repo,stdout=log,stderr=subprocess.STDOUT,timeout=900).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);return rc
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()=='0219ab2eb2466ee3e9b08064434dc9efd70053d5'
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
p=repo/'crates/server/src/broker.rs';original=p.read_bytes()
needle=b'None => self\n                .peer\n                .last\n                .checked_add(1)\n                .ok_or(Refusal::at(Phase::Authorized, Reason::IdExhausted))?,'
assert original.count(needle)==1
mutant=original.replace(needle,b'None => self.peer.last.wrapping_add(1),')
(out/'identity.json').write_text(json.dumps({'source':'0219ab2eb2466ee3e9b08064434dc9efd70053d5','original_sha256':hashlib.sha256(original).hexdigest(),'mutant_sha256':hashlib.sha256(mutant).hexdigest()},indent=2)+'\n')
try:
 p.write_bytes(mutant)
 (out/'guard-removal.diff').write_bytes(subprocess.check_output(['git','diff','--','crates/server/src/broker.rs'],cwd=repo))
 rc=run('checked-id-removed',['cargo','test','--locked','-p','shards-server','--profile','test-release','-j2','--','--exact','server::tests::broker_exhausted_caller_identity_refuses_before_packet','--nocapture'])
 log=(out/'checked-id-removed.log').read_text();print(log[-4500:],flush=True)
 assert rc==101 and 'actual_packet_id=Some(0)' in log,'real receiver identity witness absent'
finally:
 p.write_bytes(original);assert p.read_bytes()==original
 assert not subprocess.check_output(['git','diff','--name-only'],cwd=repo)
 (out/'restored.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n')
rc=run('restored-server',['cargo','test','--locked','-p','shards-server','--profile','test-release','-j2','--','--nocapture'])
print((out/'restored-server.log').read_text()[-9500:],flush=True)
if rc:sys.exit(rc)
rc=run('restored-clippy',['cargo','clippy','--locked','-p','shards-server','--all-targets','-j2','--','-D','warnings'])
print((out/'restored-clippy.log').read_text()[-2500:],flush=True)
sys.exit(rc)
