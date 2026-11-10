python3 - <<'PY'
from pathlib import Path
import os,json,hashlib,subprocess,resource,shutil,time,signal,select,tomllib
home=Path.home();root=home/'relay-turn-pressure-2300-v3'
if shutil.disk_usage(home).free<4*1024**3:raise RuntimeError('private 4GiB disk headroom')
root.mkdir(exist_ok=False);record=root/'record';record.mkdir()
os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:2]));resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));resource.setrlimit(resource.RLIMIT_CORE,(0,0))
end=time.monotonic()+600;phases=[];cases=[];error=None;before=len(os.listdir('/proc/self/fd'));source=root/'source';check=root/'check'
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def phase(name,args,cwd,env=None):
 remaining=end-time.monotonic()
 if remaining<=0:raise TimeoutError('original total600s ended')
 log=record/(name+'.log');child=None;pidfd=None;forced=False;ready=False
 with log.open('wb') as out:
  try:
   child=subprocess.Popen(args,cwd=cwd,env=env,stdout=out,stderr=subprocess.STDOUT,start_new_session=True);pidfd=os.pidfd_open(child.pid,0)
   try:status=child.wait(timeout=remaining)
   except subprocess.TimeoutExpired:forced=True;os.killpg(child.pid,signal.SIGKILL);status=child.wait()
   poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
  finally:
   if child is not None and child.returncode is None:forced=True;os.killpg(child.pid,signal.SIGKILL);child.wait()
   if pidfd is not None:os.close(pidfd)
 row={'name':name,'command':args,'status':status,'pidfd_exit_ready':ready,'reaped':child.returncode is not None,'forced_owned_cleanup':forced,'log':str(log),'log_sha256':digest(log)};phases.append(row)
 print(json.dumps({k:row[k] for k in ['name','status','pidfd_exit_ready','reaped','forced_owned_cleanup']}),flush=True)
 if status or forced or not ready:
  print(log.read_text(errors='replace')[-7000:],flush=True);raise RuntimeError(name+' failed; original record retained')
 return log
try:
 phase('clone',['git','clone','--filter=blob:none','--single-branch','--branch','p/managed-tcp-forward-a920','git@github.com:hyper-light/shards.git',str(source)],root)
 revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
 if revision!='d914908a9a2419eafd823bb5383d13ba2c275501':raise RuntimeError('exact source checkpoint')
 manifest=json.loads((source/'docs/research/measurements/managed-tcp-forward/implementation/turn-pressure/source-identities.json').read_text());expected={r['path']:r['sha256'] for r in manifest['source_closure']}
 identity={name:digest(source/name) for name in expected}
 if identity!=expected:raise RuntimeError('complete source closure before build')
 shared=(source/'crates/init/src/relay.rs').read_bytes();suffix=b'\n#[cfg(test)]\n#[path = "relay/turn_native.rs"]\nmod native_turn_tests;\n'
 if not shared.endswith(suffix) or hashlib.sha256(shared[:-len(suffix)]).hexdigest()!='083bce5ec7f0a96c74d1e44c0aa3d882344a36fce1401b017f8e114a8841fe27':raise RuntimeError('original production pump unchanged')
 (record/'identity-before-build.json').write_text(json.dumps({'revision':revision,'source_sha256':identity,'policy_revision':'b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe','policy_sha256':digest(source/'docs/development-rules.md'),'uname':list(os.uname()),'cpu_affinity':sorted(os.sched_getaffinity(0)),'bounds':{'cpu':2,'memory_bytes':2*1024**3,'disk_bytes':4*1024**3,'outer_seconds':600,'cargo_jobs':2,'case_ms':30000,'parent_ms':45000},'no_vm':True,'physical_kernel_caps':'Unverified'},indent=2))
 phase('policy',['python3','scripts/check-development-policy'],source)
 phase('test-fmt',['rustup','run','1.98.1','rustfmt','--check','--edition','2024','--config','skip_children=true',str(source/'crates/init/src/relay.rs'),str(source/'crates/init/src/relay/turn_native.rs')],source)
 check.mkdir();(source/'crates/testguest/src/interface_root.rs').write_text('#![allow(dead_code)]\nmod managed_network;\n')
 (check/'Cargo.toml').write_text('[package]\nname="relay-native-turn-check"\nversion="0.0.0"\nedition="2024"\nrust-version="1.98.1"\n[workspace]\n[lib]\npath="../source/crates/testguest/src/interface_root.rs"\n[dependencies]\nlibc="=0.2.189"\nshards-devcgroup={path="../source/crates/devcgroup"}\n')
 env=os.environ.copy();env.update({'CARGO_HOME':str(root/'cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'2','NUM_JOBS':'2','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld','CLIPPY_CONF_DIR':str(source)})
 settings=tomllib.loads((source/'Cargo.toml').read_text())['workspace']['lints'];flags=[]
 for group,items in settings.items():
  for name,value in items.items():
   level=value if isinstance(value,str) else value['level'];flags+=['--'+level,('clippy::' if group=='clippy' else '')+name]
 (record/'workspace-lints.json').write_text(json.dumps(settings,indent=2))
 for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
  phase('check-'+target,['rustup','run','1.98.1','cargo','check','--lib','--target',target,'-j','2'],check,env)
  phase('clippy-'+target,['rustup','run','1.98.1','cargo','clippy','--lib','--all-targets','--target',target,'-j','2','--',*flags,'-D','warnings'],check,env)
  log=phase('build-'+target,['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target',target,'-j','2','--message-format=json'],check,env)
  found=[]
  for line in log.read_text().splitlines():
   try:
    row=json.loads(line)
    if row.get('reason')=='compiler-artifact' and row.get('executable') and row.get('profile',{}).get('test'):found.append(row['executable'])
   except ValueError:pass
  if len(found)!=1:raise RuntimeError('one exact executable')
  frozen=root/'artifacts'/target;frozen.parent.mkdir(exist_ok=True);shutil.copy2(Path(found[0]),frozen)
  if digest(frozen)!=digest(Path(found[0])):raise RuntimeError('frozen binary copy')
  given={'parent_timeout_ms':45000,'env':{'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'5441494c0d0a','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TURN_PREFILL_BYTES':'1048576','SHARDS_TURN_PREFILL_SEED_HEX':'00ff0d0a01020304','SHARDS_NATIVE_SNDBUF_BYTES':'4096','SHARDS_NATIVE_RCVBUF_BYTES':'4096'}}
  supplied=record/(target+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
  if end-time.monotonic()<45:raise TimeoutError('insufficient original total for parent45s')
  driver=source/'docs/research/measurements/managed-tcp-forward/implementation/turn-pressure/native_case.py';place=root/'cases'/target
  phase('native-'+target,['python3',str(driver),'--case','turn-pressure','--binary',str(frozen),'--inputs',str(supplied),'--record',str(place)],source,env)
  case=json.loads((place/'result.json').read_text());observed=json.loads((place/'observed.json').read_text());cases.append({'target':target,'result':case,'observed':observed})
  if not case['passed']:raise RuntimeError('native case did not pass; no retry')
 after={name:digest(source/name) for name in expected};diff=subprocess.run(['git','diff','--exit-code'],cwd=source,capture_output=True)
 if after!=identity or diff.returncode:raise RuntimeError('source closure changed')
 (record/'source-after.json').write_text(json.dumps({'source_sha256':after,'tracked_diff_status':diff.returncode,'git_status':subprocess.check_output(['git','status','--short'],cwd=source,text=True)},indent=2))
except Exception as caught:error={'type':type(caught).__name__,'detail':str(caught)}
finally:
 result={'root':str(root),'source_revision':'d914908a9a2419eafd823bb5383d13ba2c275501','phases':phases,'cases':cases,'error':error,'fd_before':before,'fd_after':len(os.listdir('/proc/self/fd')),'no_actual_vm':True,'kernel_return':'Unknown','physical_kernel_caps':'Unverified','passed':error is None and len(cases)==2 and all(c['result']['passed'] for c in cases)}
 (record/'result.json').write_text(json.dumps(result,indent=2));print(json.dumps({'root':str(root),'phases':len(phases),'native_cases':len(cases),'error':error,'fd_before':result['fd_before'],'fd_after':result['fd_after'],'passed':result['passed']}),flush=True)
raise SystemExit(0 if result['passed'] else 1)
PY
