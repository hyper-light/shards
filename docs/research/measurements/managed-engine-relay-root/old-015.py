"""Coordinator's normal carried KVM/VSOCK engine baseline; no kernel change."""
from pathlib import Path
import hashlib,json,os,select,shutil,signal,subprocess,time
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-engine-old-015-v1";out.mkdir(exist_ok=False)
source="0150692542d7203a376a2fbcf4ad08b3ae9feb7b";sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
subprocess.run(["git","fetch","origin","p/managed-layer-production-v1"],cwd=repo,check=True,timeout=120)
subprocess.run(["git","checkout","--detach",source],cwd=repo,check=True,timeout=120)
assert sha(repo/"docs/development-rules.md")=="df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
assert sha(repo/"crates/init/src/engine.rs")=="e2e4e6e6ae014ddb82aba96362801bd14bf4a69a2955ac44941bd481f6563eea"
inputs=h/"root-managed-engine-kernel-inputs-17c-v1";config=inputs/"config-6.18.48-x86_64"
assert sha(config)=="434fdb527326d93bbbc13f65cb1bacefca0cb3df0a93f86f5ae7db0cce2b399c"
kernel=repo/"target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400";assert sha(kernel)=="48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
shutil.copy2(inputs/"inputs.json",out/"kernel-inputs.json");shutil.copy2(config,out/config.name);shutil.copy2(inputs/"SHA256SUMS",out/"SHA256SUMS")
env=os.environ.copy()
for key in ["SHARDS_HELPERS","SHARDS_HELPERS_DIR","SHARDS_INIT_BINARY","SHARDS_VM_BINARY","SHARDS_TEST_GUESTS","SHARDS_SERVER_BINARY","SHARDS_TEST_KERNEL","SHARDS_KERNEL","SHARDS_INIT"]:env.pop(key,None)
(out/"empty-docker").mkdir();(out/"frozen").mkdir()
env.update(CARGO_BUILD_JOBS="4",CARGO_TARGET_DIR=str(repo/"target"),DOCKER_CONFIG=str(out/"empty-docker"),SHARDS_NETWORK_SOURCE_REVISION=source)
def run(name,args,seconds=1800,used=None):
 (out/(name+".command.json")).write_text(json.dumps(args)+"\n");started=time.monotonic();before=len(list(Path('/proc/self/fd').iterdir()));pidfd=None;ready=False
 with (out/(name+".log")).open("wb") as log:
  p=subprocess.Popen(args,cwd=repo,env=used or env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:
   pidfd=os.pidfd_open(p.pid,0);poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(seconds*1000))
   if not ready:raise subprocess.TimeoutExpired(args,seconds)
   rc=p.wait(timeout=15);timed=False
  except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);p.wait(timeout=15);rc=124;timed=True
  finally:
   if pidfd is not None:os.close(pidfd)
 result={"exit_code":rc,"timeout":timed,"elapsed_seconds":time.monotonic()-started,"pidfd_exit_ready":ready,"direct_child_reaped":p.returncode is not None,"controller_fd_before":before,"controller_fd_after":len(list(Path('/proc/self/fd').iterdir())),"whole_population_kernel_return":"Unknown"}
 (out/(name+".result.json")).write_text(json.dumps(result,indent=2)+"\n");print(name,json.dumps(result),flush=True)
 if rc:print((out/(name+".log")).read_text(errors='replace')[-6000:],flush=True)
 assert result['controller_fd_before']==result['controller_fd_after']
 return rc
assert run('policy',['python3','scripts/check-development-policy'],60)==0
assert run('normal-carried-build',['cargo','test','--locked','-p','shards','--profile','test-release','--test','managed_network','--no-run','--message-format=json','-j4'],2400)==0
artifacts=[]
for line in (out/'normal-carried-build.log').read_text().splitlines():
 try:item=json.loads(line)
 except json.JSONDecodeError:continue
 if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='managed_network' and item.get('executable'):artifacts.append(Path(item['executable']))
assert len(artifacts)==1
guestenv=env|{'CC_x86_64_unknown_linux_musl':str(repo/'scripts/zig-cc'),'AR_x86_64_unknown_linux_musl':str(repo/'scripts/zig-ar'),'ZIG_TARGET':'x86_64-linux-musl'}
assert run('matching-static-guest',['cargo','build','--locked','-p','shards-testguest','--profile','guest-test','--target','x86_64-unknown-linux-musl','-j4'],900,guestenv)==0
guest=repo/'target/x86_64-unknown-linux-musl/guest-test/shards-testguest';prefab=out/'guests/guest';prefab.mkdir(parents=True);shutil.copy2(guest,prefab/'shards-testguest');shutil.copy2(artifacts[0],out/'frozen/managed-network-test')
identity={'source':source,'role':'coordinator','policy_sha256':sha(repo/'docs/development-rules.md'),'engine_sha256':sha(repo/'crates/init/src/engine.rs'),'shared_fixture_sha256':sha(repo/'crates/init/src/relay.rs'),'test_sha256':sha(artifacts[0]),'guest_sha256':sha(guest),'kernel_sha256':sha(kernel),'config_sha256':sha(config),'admission':'4CPU8GiB16GiB/j4; admission not hard physical or kernel cap','guest_inputs':'1CPU256MiB/case30s/boot120s/outer240s; no guessed AS cap','uname':list(os.uname()),'load':os.getloadavg()};(out/'identity.json').write_text(json.dumps(identity,indent=2)+'\n')
rows=[]
for mode in ['client-fin-first','host-fin-first']:
 case=out/mode;case.mkdir();(case/'tmp').mkdir()
 used=env|{'SHARDS_REQUIRE_VMS':'1','SHARDS_TEST_GUESTS':str(out/'guests'),'SHARDS_TEST_KERNEL':str(kernel),'SHARDS_KERNEL':str(kernel),'SHARDS_NETWORK_EFFECTIVE_CONFIG':str(config),'SHARDS_NETWORK_CONFIG_PROVENANCE':'exact release kernel-6.18.48-17c3ba205400 effective config; release digest and SHA256SUMS checked; no guest config readback','SHARDS_NETWORK_RECORDS':str(case/'records'),'SHARDS_NETWORK_BOOT_MS':'120000','SHARDS_NETWORK_CASE_MS':'30000','SHARDS_NETWORK_MEMORY_MIB':'256','SHARDS_NETWORK_CPUS':'1','SHARDS_ENGINE_HALF_MODE':mode,'SHARDS_ENGINE_REQUEST_HEX':b'GET /_ping HTTP/1.1\r\nHost: engine\r\nConnection: close\r\n\r\n'.hex(),'SHARDS_ENGINE_SUFFIX_HEX':b'OK'.hex(),'SHARDS_ENGINE_LATE_HEX':b'X'.hex() if mode=='host-fin-first' else '', 'SHARDS_ENGINE_RESPONSE_BYTES':'4096','TMPDIR':str(case/'tmp'),'XDG_CACHE_HOME':str(case/'runtime-cache'),'SHARDS_CACHE':str(case/'runtime-cache/shards')}
 (case/'inputs.json').write_text(json.dumps({k:v for k,v in used.items() if k.startswith('SHARDS_') or k=='TMPDIR'},indent=2)+'\n')
 rc=run(mode,[str(artifacts[0]),'tcp_forward::managed_engine_half_close_records_actual_host_finish','--exact','--ignored','--nocapture','--test-threads=1'],240,used)
 text=(out/(mode+'.log')).read_text(errors='replace');assert rc==0 and '1 passed; 0 failed; 0 ignored;' in text and 'SKIP:' not in text
 observed=json.loads((case/'records/engine.observed.json').read_bytes());record=[r for r in observed if r['type']=='EngineHalfClose'];assert len(record)==1
 rows.append({'mode':mode,'actual':record[0]});print(mode,json.dumps(record[0]),flush=True)
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
(out/'completed.json').write_text(json.dumps({'source':source,'old_engine_unchanged':True,'cases':rows,'time_unix_ns':time.time_ns(),'scope':'actual carried old-engine VSOCK facts; no candidate equivalence, interception, whole-population or performance claim'},indent=2)+'\n');print('COMPLETE',source,str(out),flush=True)
