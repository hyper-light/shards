python3 - <<'PY'
from pathlib import Path
import os,sys,json,hashlib,subprocess,resource,shutil,time,signal,select,tomllib
home=Path.home();root=home/'relay-tcp-extra-backend-f6a-v1'
if shutil.disk_usage(home).free<4*1024**3:raise RuntimeError('private 4GiB disk headroom')
root.mkdir(exist_ok=False);record=root/'record';record.mkdir()
os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:2]))
resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));resource.setrlimit(resource.RLIMIT_CORE,(0,0))
end=time.monotonic()+600;phases=[];cases=[];before_fd=len(os.listdir('/proc/self/fd'));error=None;restoration=None
source=root/'source';check=root/'check';artifacts=root/'artifacts';artifacts.mkdir()
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def phase(name,args,cwd,env=None,expected=0):
 remaining=end-time.monotonic()
 if remaining<=0:raise TimeoutError('caller total600s elapsed')
 log=record/(name+'.log');child=None;pidfd=None;forced=False;ready=False
 with log.open('wb') as output:
  try:
   child=subprocess.Popen(args,cwd=cwd,env=env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True);pidfd=os.pidfd_open(child.pid,0)
   try:status=child.wait(timeout=remaining)
   except subprocess.TimeoutExpired:forced=True;os.killpg(child.pid,signal.SIGKILL);status=child.wait()
   poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
  finally:
   if child is not None and child.returncode is None:forced=True;os.killpg(child.pid,signal.SIGKILL);child.wait()
   if pidfd is not None:os.close(pidfd)
 row={'name':name,'command':args,'status':status,'expected_status':expected,'pidfd_exit_ready':ready,'reaped':child.returncode is not None,'forced_owned_cleanup':forced,'log':str(log),'log_sha256':digest(log)}
 phases.append(row);print(json.dumps({k:row[k] for k in ['name','status','expected_status','pidfd_exit_ready','reaped','forced_owned_cleanup']}),flush=True)
 if status!=expected or not ready or forced:
  print(log.read_text(errors='replace')[-5000:],flush=True);raise RuntimeError(name+' failed; retained raw')
 return log
def native_build(label,target):
 log=phase(label,['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target',target,'-j','2','--message-format=json'],check,env)
 found=[]
 for line in log.read_text().splitlines():
  try:
   row=json.loads(line)
   if row.get('reason')=='compiler-artifact' and row.get('executable') and row.get('profile',{}).get('test'):found.append(row['executable'])
  except ValueError:pass
 if len(found)!=1:raise RuntimeError('one exact native executable')
 binary=Path(found[0]);frozen=artifacts/label;shutil.copy2(binary,frozen)
 if digest(frozen)!=digest(binary):raise RuntimeError('frozen executable identity')
 return frozen
def native_case(name,binary,kind,scenario='server-first',mutant=False):
 if end-time.monotonic()<45:raise TimeoutError('insufficient original total for next parent45s case')
 given={'parent_timeout_ms':45000,'env':{'SHARDS_TCP_SCENARIO':scenario,'SHARDS_ENGINE_HALF':'client-fin-first','SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'5441494c0d0a','SHARDS_TCP_STORAGE':'caller','SHARDS_TCP_EXTRA_HEX':'58'}}
 supplied=record/(name+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
 place=root/'cases'/name
 phase(name,['python3',str(driver),'--case',kind,'--binary',str(binary),'--inputs',str(supplied),'--record',str(place)],source,env,1 if mutant else 0)
 result=json.loads((place/'result.json').read_text());rows=json.loads((place/'observed.json').read_text())
 row={'name':name,'mutant':mutant,'result':result,'observed':rows,'input_sha256':digest(supplied)}
 if mutant:
  endpoints=[x for x in rows if x.get('type')=='Endpoint'];client=[x for x in endpoints if x.get('role')=='NativeClient'];backend=[x for x in endpoints if x.get('role')=='NativeBackend'];receipt=[x for x in endpoints if x.get('role')=='BackendBoundaryReceipt'];flow=[x for x in rows if x.get('type')=='NativeFlow'];cleanup=[x for x in rows if x.get('type')=='NativeBoundaryCleanup'];joined=[x for x in rows if x.get('type')=='NativeJoined']
  response='53455256455200ff5441494c0d0a';request='5245515545535400ff0d0a0102'
  witness={'strict_native101':result.get('status')==101,'actual_client_extra58':len(client)==1 and client[0]['extra_hex']=='58' and client[0]['read_hex']==response and client[0]['read_end']==client[0]['write_end']=='Closed' and not client[0]['ok'],'backend_actual_request_eof_plus15written':len(backend)==1 and backend[0]['ok'] and backend[0]['read_hex']==request and backend[0]['read_end']==backend[0]['write_end']=='Closed' and backend[0]['written']==15,'held_backend_prefix':len(receipt)==1 and receipt[0]['ok'] and receipt[0]['read_hex']==request and receipt[0]['read_end']=='Closed' and receipt[0]['write_end']=='Open' and receipt[0]['written']==14,'forwarded_extra_actual_count':len(flow)==1 and flow[0]['fault'] is None and flow[0]['end']=='BothHalvesClosed' and flow[0]['to_client']['read']==flow[0]['to_client']['written']==15 and flow[0]['to_client']['pending']==flow[0]['to_client']['extra_consumed']==0 and flow[0]['to_backend']['read']==flow[0]['to_backend']['written']==13,'child_fd_return_before_assert':len(cleanup)==1 and cleanup[0]['fd_before']==cleanup[0]['fd_after'],'joined_before_original_deadline':len(joined)==1 and joined[0]['relay_joined'] and joined[0]['backend_joined'] and joined[0]['original_remaining_ns']>0,'parent_return_and_binary':result['fd_before']==result['fd_after'] and result['pidfd_exit_ready'] and result['reaped'] and not result['timeout'] and result['binary_before_sha256']==result['binary_after_sha256']}
  row['forbidden_effect_checks']=witness;row['expected_outcome']=all(witness.values()) and not result['passed']
 else:row['expected_outcome']=result['passed']
 cases.append(row)
 if not row['expected_outcome']:raise RuntimeError(name+' missing actual expected outcome')
try:
 phase('clone',['git','clone','--filter=blob:none','--single-branch','--branch','p/managed-tcp-forward-a920','git@github.com:hyper-light/shards.git',str(source)],root)
 revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
 if revision!='5a4b676aa4d4f83f29fa88e6512fb3edb5ce2e62':raise RuntimeError('exact source checkpoint')
 manifest=json.loads((source/'docs/research/measurements/managed-tcp-forward/implementation/current-main/source-identities.json').read_text())
 expected={x['path']:x['sha256'] for x in manifest['paths']}
 fixture='crates/testguest/src/managed_network/intercept/tcp_forward.rs';expected[fixture]='d076019b672558ef53a8a8168f9b4989a08205972501cd6de62ed857e0c03e66'
 identities={name:digest(source/name) for name in expected}
 if identities!=expected:raise RuntimeError('exact seven-source identities except added cfgtest')
 if digest(source/'docs/development-rules.md')!='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3':raise RuntimeError('policy identity')
 (record/'identity.json').write_text(json.dumps({'source_revision':revision,'policy_revision':'b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe','rules_sha256':digest(source/'docs/development-rules.md'),'source_sha256':identities,'uname':list(os.uname()),'cpus':sorted(os.sched_getaffinity(0)),'bounds':{'cpu':2,'memory_bytes':2*1024**3,'disk_bytes':4*1024**3,'outer_seconds':600,'cargo_jobs':2,'case_ms':30000,'parent_ms':45000},'no_actual_vm':True,'physical_kernel_caps':'Unverified'},indent=2))
 phase('policy',['python3','scripts/check-development-policy'],source)
 phase('fixture-fmt',['rustup','run','1.98.1','rustfmt','--check','--edition','2024','--config','skip_children=true',str(source/fixture)],source)
 check.mkdir();(source/'crates/testguest/src/interface_root.rs').write_text('#![allow(dead_code)]\nmod managed_network;\n')
 (check/'Cargo.toml').write_text('[package]\nname="relay-extra-backend-check"\nversion="0.0.0"\nedition="2024"\nrust-version="1.98.1"\n[workspace]\n[lib]\npath="../source/crates/testguest/src/interface_root.rs"\n[dependencies]\nlibc="=0.2.189"\nshards-devcgroup={path="../source/crates/devcgroup"}\n')
 driver=source/'docs/research/measurements/managed-tcp-forward/implementation/extra-backend-byte/native_cases.py'
 if digest(driver)!='5330cc8f2859034bf3a6489ec7d0a406d1efe299a42cc6af7e30168e3e13c384':raise RuntimeError('driver identity')
 env=os.environ.copy();env.update({'CARGO_HOME':str(root/'cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'2','NUM_JOBS':'2','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld','CLIPPY_CONF_DIR':str(source)})
 settings=tomllib.loads((source/'Cargo.toml').read_text())['workspace']['lints'];flags=[]
 for group,items in settings.items():
  for name,value in items.items():
   level=value if isinstance(value,str) else value['level'];flags+=['--'+level,('clippy::' if group=='clippy' else '')+name]
 (record/'workspace-lints.json').write_text(json.dumps(settings,indent=2))
 for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
  phase('check-'+target,['rustup','run','1.98.1','cargo','check','--lib','--target',target,'-j','2'],check,env)
  phase('clippy-'+target,['rustup','run','1.98.1','cargo','clippy','--lib','--all-targets','--target',target,'-j','2','--',*flags,'-D','warnings'],check,env)
  baseline=native_build('baseline-'+target,target)
  native_case('baseline-'+target,baseline,'extra-backend-byte')
 changed=source/fixture;original=changed.read_bytes();anchor=b'        limits.map(ByteLimit::Bytes),\n'
 replacement=b'        {\n            let [request, _] = limits;\n            [ByteLimit::Bytes(request), ByteLimit::PeerEnd]\n        },\n'
 if original.count(anchor)!=1:raise RuntimeError('one exact response ByteLimit call site')
 try:
  changed.write_bytes(original.replace(anchor,replacement));(record/'mutant-fixture.rs').write_bytes(changed.read_bytes())
  (record/'mutation.patch').write_bytes(subprocess.check_output(['git','diff','--',fixture],cwd=source))
  mutant=native_build('mutant-x86_64-unknown-linux-musl','x86_64-unknown-linux-musl')
  native_case('mutant-response-limit-musl',mutant,'extra-backend-byte',mutant=True)
 finally:
  changed.write_bytes(original)
  after={name:digest(source/name) for name in expected};diff=subprocess.run(['git','diff','--exit-code'],cwd=source,capture_output=True)
  restoration={'original_fixture_sha256':hashlib.sha256(original).hexdigest(),'restored_fixture_sha256':digest(changed),'all_source_before':identities,'all_source_after':after,'tracked_diff_status':diff.returncode,'tracked_diff_bytes':len(diff.stdout),'exact':original==changed.read_bytes() and identities==after and diff.returncode==0}
  (record/'restoration.json').write_text(json.dumps(restoration,indent=2))
  if not restoration['exact']:raise RuntimeError('exact restoration failed')
 for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
  restored=native_build('restored-'+target,target)
  for label,kind,scenario in [('backend-byte','extra-backend-byte','server-first'),('server-first','both-half-closes','server-first'),('backend-fin-first','both-half-closes','backend-fin-first'),('client-byte','extra-client-byte','server-first'),('receipt-cancel','receipt-cancel','server-first')]:
   native_case('restored-'+target+'-'+label,restored,kind,scenario)
 after={name:digest(source/name) for name in expected};diff=subprocess.run(['git','diff','--exit-code'],cwd=source,capture_output=True)
 if after!=identities or diff.returncode!=0:raise RuntimeError('final exact source identities')
 (record/'source-after.json').write_text(json.dumps({'source_sha256':after,'tracked_diff_status':diff.returncode,'git_status':subprocess.check_output(['git','status','--short'],cwd=source,text=True)},indent=2))
except Exception as caught:error={'type':type(caught).__name__,'detail':str(caught)}
finally:
 result={'root':str(root),'source_revision':'5a4b676aa4d4f83f29fa88e6512fb3edb5ce2e62','phases':phases,'cases':cases,'restoration':restoration,'error':error,'fd_before':before_fd,'fd_after':len(os.listdir('/proc/self/fd')),'no_actual_vm':True,'kernel_return':'Unknown','physical_kernel_caps':'Unverified','passed':error is None and len(cases)==13 and all(r['expected_outcome'] for r in cases)}
 (record/'result.json').write_text(json.dumps(result,indent=2))
 print(json.dumps({'root':str(root),'phases':len(phases),'cases':len(cases),'positive':sum(not r['mutant'] for r in cases),'expected_actual_effect_mutants':sum(r['mutant'] and r['expected_outcome'] for r in cases),'restored_exact':restoration and restoration['exact'],'error':error,'fd_before':result['fd_before'],'fd_after':result['fd_after'],'passed':result['passed']}),flush=True)
raise SystemExit(0 if result['passed'] else 1)
PY
