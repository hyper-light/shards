import pathlib, subprocess, json, hashlib, os, shutil, sys, time
repo=pathlib.Path.home()/'managed-capture-check'
out=pathlib.Path.home()/'root-managed-program-admission-before-v1'
out.mkdir(exist_ok=False)
def run(name, argv, timeout):
 (out/(name+'.command.json')).write_text(json.dumps(argv)+'\n')
 with (out/(name+'.log')).open('wb') as f:
  try: rc=subprocess.run(argv,cwd=repo,env=os.environ.copy(),stdout=f,stderr=subprocess.STDOUT,timeout=timeout).returncode
  except subprocess.TimeoutExpired: rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n')
 print(name,rc,flush=True)
 return rc
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
if run('fetch',['git','fetch','origin','p/managed-program-admission-v1'],120): sys.exit(1)
if run('checkout',['git','checkout','--detach','origin/p/managed-program-admission-v1'],120): sys.exit(1)
sha=subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()
(out/'identity.json').write_text(json.dumps({'source':sha,'disk_free':shutil.disk_usage(repo).free,'command_scope':'root-only server exact native caller','policy_sha256':'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3'},indent=2)+'\n')
run('toolchain',['rustc','--version','--verbose'],60)
rc=run('baseline',['cargo','test','-p','shards-server','--profile','test-release','-j','2','an_exhausted_program_identity_cannot_release_another_worker','--','--exact','server::tests::an_exhausted_program_identity_cannot_release_another_worker','--nocapture'],1800)
print((out/'baseline.log').read_text()[-9000:],flush=True)
print('evidence',str(out),flush=True)
sys.exit(rc)
