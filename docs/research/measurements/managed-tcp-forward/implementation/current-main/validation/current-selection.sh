python3 - <<'PY'
from pathlib import Path
import os,sys,json,hashlib,subprocess,resource,shutil,time,signal,select,tomllib,gzip,base64
home=Path.home();root=home/'relay-current-1be-a920-light-v2'
if shutil.disk_usage(home).free<4*1024**3:raise RuntimeError('private 4GiB disk headroom')
root.mkdir(exist_ok=False);record=root/'record';record.mkdir()
os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:2]))
resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));resource.setrlimit(resource.RLIMIT_CORE,(0,0))
end=time.monotonic()+600
results=[];native=[];before_fd=len(os.listdir('/proc/self/fd'));error=None
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def phase(name,args,cwd,env=None):
 remaining=end-time.monotonic()
 if remaining<=0:raise TimeoutError('caller total600s elapsed before phase')
 log=record/(name+'.log');child=None;pidfd=None;forced=False
 with log.open('wb') as output:
  try:
   child=subprocess.Popen(args,cwd=cwd,env=env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
   pidfd=os.pidfd_open(child.pid,0)
   try:status=child.wait(timeout=remaining)
   except subprocess.TimeoutExpired:
    forced=True;os.killpg(child.pid,signal.SIGKILL);status=child.wait()
   poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
  finally:
   if child is not None and child.returncode is None:
    forced=True;os.killpg(child.pid,signal.SIGKILL);child.wait()
   if pidfd is not None:os.close(pidfd)
 row={'name':name,'command':args,'status':status,'pidfd_exit_ready':ready,'reaped':True,'forced_owned_cleanup':forced,'log':str(log),'log_sha256':digest(log)}
 results.append(row);print(json.dumps(row),flush=True)
 if status:
  print(log.read_text(errors='replace')[-12000:],flush=True)
  raise RuntimeError(name+' failed; retained raw log')
 return log
source=root/'source';check=root/'check'
try:
 phase('clone',['git','clone','--filter=blob:none','--single-branch','--branch','p/managed-tcp-forward-a920','git@github.com:hyper-light/shards.git',str(source)],root)
 revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
 if revision!='1be384ecb3e3852a6b97daaf1ead61d441f21d20':raise RuntimeError('exact approved revision')
 manifest=json.loads((source/'docs/research/measurements/managed-tcp-forward/implementation/current-main/source-identities.json').read_text())
 for item in manifest['paths']:
  if digest(source/item['path'])!=item['sha256']:raise RuntimeError('selected byte identity')
 rules=digest(source/'docs/development-rules.md')
 if rules!='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3':raise RuntimeError('policy identity')
 identity={'revision':revision,'base':manifest['base'],'policy_sha256':rules,'source':manifest['paths'],'runner_kernel':os.uname().release,'machine':os.uname().machine,'cpu_affinity':sorted(os.sched_getaffinity(0)),'scope':'normal init check/strict lint plus guest managed_network projection and existing native corpus; no VM; headroom not physical/kernel hardcap','bounds':{'cpus':2,'memory_bytes':2*1024**3,'disk_bytes':4*1024**3,'outer_seconds':600,'cargo_jobs':2}}
 (record/'identity.json').write_text(json.dumps(identity,indent=2))
 phase('policy',['python3','scripts/check-development-policy'],source)
 rustfiles=[str(source/item['path']) for item in manifest['paths'] if item['path'].endswith('.rs')]
 phase('selected-fmt',['rustup','run','1.98.1','rustfmt','--check','--edition','2024','--config','skip_children=true',*rustfiles],source)
 phase('host-registration-check',['git','apply','--check','docs/research/measurements/managed-tcp-forward/implementation/current-main/host-registration.patch'],source)
 driver=record/'original-native_cases.py'
 driver.write_bytes(gzip.decompress(base64.b64decode(os.environ['RELAY_RETAINED_DRIVER'])))
 if digest(driver)!='eef969298d83c23dc6247131c9ae14078971ab732b47f0c25864f55218838fd9':raise RuntimeError('original driver identity')
 settings=tomllib.loads((source/'Cargo.toml').read_text())['workspace']['lints']
 flags=[]
 for group,items in settings.items():
  for name,value in items.items():
   level=value if isinstance(value,str) else value['level']
   flags+=['--'+level,('clippy::' if group=='clippy' else '')+name]
 check.mkdir()
 (source/'crates/testguest/src/interface_root.rs').write_text('#![allow(dead_code)]\nmod managed_network;\n')
 (check/'Cargo.toml').write_text('[package]\nname="relay-current-source-check"\nversion="0.0.0"\nedition="2024"\nrust-version="1.98.1"\n[workspace]\n[lib]\npath="../source/crates/testguest/src/interface_root.rs"\n[dependencies]\nlibc="=0.2.189"\nshards-devcgroup={path="../source/crates/devcgroup"}\n')
 env=os.environ.copy();env.update({'CARGO_HOME':str(root/'cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'2','NUM_JOBS':'2','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld','CLIPPY_CONF_DIR':str(source)})
 (record/'check-manifest.toml').write_bytes((check/'Cargo.toml').read_bytes())
 (record/'check-root.rs').write_bytes((source/'crates/testguest/src/interface_root.rs').read_bytes())
 (record/'clippy-config.toml').write_bytes((source/'clippy.toml').read_bytes())
 (record/'workspace-lints.json').write_text(json.dumps(settings,indent=2))
 for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
  phase('init-check-'+target,['rustup','run','1.98.1','cargo','check','--locked','-p','shards-init','--bin','shards-init','--target',target,'-j','2'],source,env)
  phase('init-clippy-'+target,['rustup','run','1.98.1','cargo','clippy','--locked','-p','shards-init','--bin','shards-init','--target',target,'-j','2','--','-D','warnings'],source,env)
  phase('guest-module-check-'+target,['rustup','run','1.98.1','cargo','check','--lib','--target',target,'-j','2'],check,env)
  phase('guest-module-clippy-'+target,['rustup','run','1.98.1','cargo','clippy','--lib','--all-targets','--target',target,'-j','2','--',*flags,'-D','warnings'],check,env)
  log=phase('native-build-'+target,['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target',target,'-j','2','--message-format=json'],check,env)
  artifacts=[]
  for line in log.read_text().splitlines():
   try:
    item=json.loads(line)
    if item.get('reason')=='compiler-artifact' and item.get('executable') and item.get('profile',{}).get('test'):artifacts.append(item['executable'])
   except ValueError:pass
  if len(artifacts)!=1:raise RuntimeError('one exact native test binary')
  binary=Path(artifacts[0]);frozen=root/'artifacts'/target;frozen.parent.mkdir(exist_ok=True)
  shutil.copy2(binary,frozen)
  if digest(binary)!=digest(frozen):raise RuntimeError('frozen artifact')
  cases=[('unix-client','engine-unix','server-first','client-fin-first',False),('unix-host','engine-unix','server-first','host-fin-first',False),('tcp-server','both-half-closes','server-first','client-fin-first',False),('tcp-backend-fin','both-half-closes','backend-fin-first','host-fin-first',False),('extra-client','extra-client-byte','server-first','client-fin-first',False),('cancel','receipt-cancel','server-first','client-fin-first',False),('backend-reset','reset','server-first','client-fin-first',False),('client-reset','client-reset','server-first','client-fin-first',False),('pressure-server','both-half-closes','server-first','client-fin-first',True),('pressure-backend-fin','both-half-closes','backend-fin-first','host-fin-first',True)]
  for name,kind,scenario,half,pressure in cases:
   if end-time.monotonic()<45:raise TimeoutError('insufficient remaining total bound for next owned case')
   name=target+'-'+name
   given={'parent_timeout_ms':45000,'env':{'SHARDS_TCP_SCENARIO':scenario,'SHARDS_ENGINE_HALF':half,'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'65536' if pressure else '3','SHARDS_TCP_DOWN_BYTES':'65536' if pressure else '5','SHARDS_TCP_REQUEST_HEX':'repeat:1048576:00ff0d0a01020304' if pressure else '5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'repeat:131072:5441494c00ff' if pressure else '5441494c0d0a','SHARDS_TCP_STORAGE':'caller','SHARDS_TCP_EXTRA_HEX':'58'}}
   if pressure:given['env'].update({'SHARDS_NATIVE_WRITE_PRESSURE':'required','SHARDS_NATIVE_SNDBUF_BYTES':'4096','SHARDS_NATIVE_RCVBUF_BYTES':'4096'})
   supplied=record/(name+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
   phase(name,['python3',str(driver),'--case',kind,'--binary',str(frozen),'--inputs',str(supplied),'--record',str(root/'cases'/name)],source,env)
   outcome=json.loads((root/'cases'/name/'result.json').read_text())
   if not outcome.get('passed'):raise RuntimeError('native case did not pass')
   rows=json.loads((root/'cases'/name/'observed.json').read_text())
   compact=[]
   for row in rows:
    if row.get('type')=='Endpoint':
     data=bytes.fromhex(row['read_hex']);row={k:v for k,v in row.items() if k!='read_hex'}|{'read_bytes':len(data),'read_sha256':hashlib.sha256(data).hexdigest()}
    compact.append(row)
   native.append({'name':name,'pressure':pressure,'result':outcome,'observed':compact})
 phase('tracked-source-unchanged',['git','diff','--exit-code'],source)
 after=[]
 for item in manifest['paths']:
  actual=digest(source/item['path'])
  if actual!=item['sha256']:after.append({'path':item['path'],'actual':actual})
 if after:raise RuntimeError('selected source changed')
except Exception as caught:
 error={'type':type(caught).__name__,'detail':str(caught)}
finally:
 result={'root':str(root),'source_revision':'1be384ecb3e3852a6b97daaf1ead61d441f21d20','phases':results,'native':native,'error':error,'fd_before':before_fd,'fd_after':len(os.listdir('/proc/self/fd')),'kernel_return':'Unknown','physical_kernel_caps':'Unverified','no_actual_vm':True,'passed':error is None and len(native)==20 and all(r['status']==0 and not r['forced_owned_cleanup'] for r in results)}
 (record/'result.json').write_text(json.dumps(result,indent=2))
 print(json.dumps({'root':str(root),'phases':len(results),'native_cases':len(native),'pressure_cases':sum(r['pressure'] for r in native),'error':error,'fd_before':result['fd_before'],'fd_after':result['fd_after'],'passed':result['passed']}),flush=True)
raise SystemExit(0 if result['passed'] else 1)
PY
