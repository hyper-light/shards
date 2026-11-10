from pathlib import Path
import subprocess, os, json, hashlib, shutil, sys
repo=Path.home()/'managed-capture-check';out=Path.home()/'root-managed-program-admission-kvm-v1';out.mkdir(exist_ok=False)
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()=='3b1967838b140466ff3367637541492a77be0459'
assert not subprocess.check_output(['git','diff','--name-only'],cwd=repo)
env=os.environ.copy()
for name in ['SHARDS_HELPERS','SHARDS_HELPERS_DIR','SHARDS_INIT_BINARY','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS','SHARDS_SERVER_BINARY']:env.pop(name,None)
(out/'empty-docker').mkdir()
env.update(DOCKER_CONFIG=str(out/'empty-docker'),CARGO_BUILD_JOBS='4')
(out/'identity.json').write_text(json.dumps({'source':'3b1967838b140466ff3367637541492a77be0459','disk_free':shutil.disk_usage(repo).free,'kernel_vm_available':os.access('/dev/kvm',os.R_OK|os.W_OK),'server_sha256':hashlib.sha256((repo/'crates/server/src/server.rs').read_bytes()).hexdigest()},indent=2)+'\n')
cmd=['cargo','test','--locked','-j','4','--profile','test-release','-p','shards','--test','build','code_mode_runs_a_program_with_the_callers_grants','--','--exact','--nocapture']
(out/'command.json').write_text(json.dumps(cmd)+'\n')
with (out/'kvm.log').open('wb') as f:
 try:rc=subprocess.run(cmd,cwd=repo,env=env,stdout=f,stderr=subprocess.STDOUT,timeout=3600).returncode
 except subprocess.TimeoutExpired:rc=124
(out/'kvm.exit').write_text(str(rc)+'\n')
print('actual-full-build-kvm',rc,flush=True)
print((out/'kvm.log').read_text()[-12000:],flush=True)
sys.exit(rc)
