from pathlib import Path
import subprocess,json,os,hashlib,shutil,sys
h=Path.home();repo=h/'managed-capture-check';out=h/'root-managed-layer-combined-f1a3-v1';out.mkdir(exist_ok=False)
env=os.environ.copy()
for k in ['SHARDS_HELPERS','SHARDS_HELPERS_DIR','SHARDS_INIT_BINARY','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS','SHARDS_SERVER_BINARY']:env.pop(k,None)
(out/'empty-docker').mkdir();env.update(DOCKER_CONFIG=str(out/'empty-docker'),CARGO_BUILD_JOBS='4')
def run(name,args,seconds=1800):
 (out/(name+'.command.json')).write_text(json.dumps(args)+'\n')
 with (out/(name+'.log')).open('wb') as log:
  try:rc=subprocess.run(args,cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=seconds).returncode
  except subprocess.TimeoutExpired:rc=124
 (out/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);return rc
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
assert run('fetch',['git','fetch','origin','p/managed-layer-production-v1'],120)==0
assert run('checkout',['git','checkout','--detach','f1a3e562796b4635359a4e19df3c8c2416339782'],120)==0
assert os.access('/dev/kvm',os.R_OK|os.W_OK)
(out/'identity.json').write_text(json.dumps({'source':'f1a3e562796b4635359a4e19df3c8c2416339782','free_bytes':shutil.disk_usage(repo).free,'role':'coordinator','policy':'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3','owned_clone':str(repo),'envelope':'4CPU/8GiB/16GiB'},indent=2)+'\n')
assert run('server-native',['cargo','test','--locked','-p','shards-server','--profile','test-release','-j4','--','--nocapture'])==0
rc=run('normal-carried-build',['cargo','test','--locked','-p','shards','--profile','test-release','--test','managed_output','--no-run','--message-format=json','-j4'],2400)
if rc:print((out/'normal-carried-build.log').read_text()[-8000:]);sys.exit(rc)
artifacts=[]
for line in (out/'normal-carried-build.log').read_text().splitlines():
 try:item=json.loads(line)
 except json.JSONDecodeError:continue
 if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='managed_output' and item.get('executable'):artifacts.append(item)
assert len(artifacts)==1,artifacts
binary=Path(artifacts[0]['executable'])
assert binary.is_file()
(out/'compiler-artifact.json').write_text(json.dumps(artifacts[0],indent=2)+'\n')
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
(out/'artifacts.json').write_text(json.dumps([{'path':str(p),'bytes':p.stat().st_size,'sha256':digest(p)} for p in [binary,repo/'crates/server/src/server.rs',repo/'crates/server/src/sandbox.rs',repo/'crates/server/src/broker.rs',repo/'crates/init/src/domains/admission.rs']],indent=2)+'\n')
# Build the actual matching static testguest, retaining normal carried helpers.
guest_env=env.copy();guest_env.update(CC_x86_64_unknown_linux_musl=str(repo/'scripts/zig-cc'),AR_x86_64_unknown_linux_musl=str(repo/'scripts/zig-ar'),ZIG_TARGET='x86_64-linux-musl')
(out/'guest.command.json').write_text(json.dumps(['cargo','build','--locked','-p','shards-testguest','--profile','guest-test','--target','x86_64-unknown-linux-musl','-j4'])+'\n')
with (out/'guest.log').open('wb') as log:rc=subprocess.run(['cargo','build','--locked','-p','shards-testguest','--profile','guest-test','--target','x86_64-unknown-linux-musl','-j4'],cwd=repo,env=guest_env,stdout=log,stderr=subprocess.STDOUT,timeout=900).returncode
(out/'guest.exit').write_text(str(rc)+'\n');print('static-guest',rc,flush=True)
if rc:sys.exit(rc)
guests=out/'guests/guest';guests.mkdir(parents=True)
shutil.copy2(repo/'target/x86_64-unknown-linux-musl/guest-test/shards-testguest',guests/'shards-testguest')
cmd=['python3',str(repo/'docs/research/measurements/managed-broker-effects/caller.py'),'--source',str(repo),'--revision','f1a3e562796b4635359a4e19df3c8c2416339782','--binary',str(binary),'--records',str(out/'actual-kvm'),'--guests',str(out/'guests'),'--leaf-sha','74d459d44f11f5bbe4dfe46611aa35c7e276c150001c7d6f198456b0adeabcaf','--case-ms','180000','--call-ms','120000','--cleanup-ms','15000','--timeout-ms','25','--cancel-work-ms','1000','--capture-bytes','65536','--outer-ms','240000','--reap-ms','15000']
rc=run('actual-kvm',cmd,300)
print((out/'actual-kvm.log').read_text()[-6000:],flush=True)
if (out/'actual-kvm/native.log').exists():print((out/'actual-kvm/native.log').read_text()[-6500:],flush=True)
sys.exit(rc)
