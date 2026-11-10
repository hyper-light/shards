from pathlib import Path
import subprocess,json,hashlib,sys
repo=Path.home()/'managed-capture-check';out=Path.home()/'root-managed-program-native-mutations-v1';out.mkdir(exist_ok=False)
def run(name,args,seconds=900):
 (out/(name+'.command.json')).write_text(json.dumps(args)+'\n')
 with (out/(name+'.log')).open('wb') as f:
  try:rc=subprocess.run(args,cwd=repo,stdout=f,stderr=subprocess.STDOUT,timeout=seconds).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);return rc
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
assert run('fetch',['git','fetch','origin','p/managed-layer-production-v1'],120)==0
assert run('checkout',['git','checkout','--detach','3cc9f6dbaf5f7226b509502a64b639c8d71c617e'],120)==0
p=repo/'crates/server/src/server.rs';original=p.read_bytes()
(out/'source.txt').write_text(subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True))
(out/'original-server.sha256').write_text(hashlib.sha256(original).hexdigest()+'\n')
cases=[
 ('control-guard-removed',b'if unsafe { libc::fcntl(control.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {',b'if unsafe { libc::fcntl(control.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 && false {','server::tests::an_unusable_helper_control_is_not_admitted','unusable_native_control_rejected=false'),
 ('display-guard-removed',b'write!(out, "{:?} (OS {:?})", self.kind, self.code)',b'match self.code { Some(code) => std::fmt::Display::fmt(&io::Error::from_raw_os_error(code), out), None => write!(out, "{:?}", self.kind) }','server::tests::a_typed_program_refusal_formats_without_requesting_storage','actual_refusal_display_allocations=1')
]
for name,needle,replacement,test,receipt in cases:
 assert original.count(needle)==1
 mutant=original.replace(needle,replacement)
 (out/(name+'.sha256')).write_text(hashlib.sha256(mutant).hexdigest()+'\n')
 (out/(name+'.patch')).write_text(subprocess.run(['git','diff','--no-index','/dev/null','/dev/null'],capture_output=True,text=True).stdout)
 try:
  p.write_bytes(mutant)
  (out/(name+'.diff')).write_bytes(subprocess.check_output(['git','diff','--','crates/server/src/server.rs'],cwd=repo))
  rc=run(name,['cargo','test','--locked','-p','shards-server','--profile','test-release','-j','2','--','--exact',test,'--nocapture'])
  log=(out/(name+'.log')).read_text();print(log[-4000:],flush=True)
  assert rc==101,'mutation did not fail as expected'
  assert receipt in log,'exact actual witness absent'
 finally:
  p.write_bytes(original)
  assert p.read_bytes()==original
  assert not subprocess.check_output(['git','diff','--name-only'],cwd=repo)
  (out/(name+'.restored.sha256')).write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n')
rc=run('restored-server',['cargo','test','--locked','-p','shards-server','--profile','test-release','-j','2','--','--nocapture'])
print((out/'restored-server.log').read_text()[-9000:],flush=True)
if rc:sys.exit(rc)
rc=run('restored-clippy',['cargo','clippy','--locked','-p','shards-server','--all-targets','-j','2','--','-D','warnings'])
print((out/'restored-clippy.log').read_text()[-4000:],flush=True)
sys.exit(rc)
