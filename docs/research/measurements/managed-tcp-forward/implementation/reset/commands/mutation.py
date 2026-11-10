from pathlib import Path
import os,sys,subprocess,resource,shutil,json,hashlib
root=Path.home()/'relay-tcp-reset-mutation-v1';root.mkdir(exist_ok=False)
if shutil.disk_usage(root).free < 2*1024**3: raise RuntimeError('private disk floor')
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]});resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
original=Path.home()/'relay-tcp-forward-cbd3-light-v9'
source=root/'source';check=root/'check';shutil.copytree(original/'source',source);shutil.copytree(original/'check',check)
before={str(p.relative_to(source)):hashlib.sha256(p.read_bytes()).hexdigest() for p in source.rglob('*') if p.is_file()}
shared=source/'crates/testguest/src/managed_network/intercept/tcp_forward.rs';exact=shared.read_bytes()
(root/'before-shared.rs').write_bytes(exact)
needle=b'    report.abort = reset_peer(&result, [&downstream, &upstream]);'
if exact.count(needle)!=1:raise RuntimeError('one exact TCP finalizer seam required')
removed=b'    report.abort = Abort::NotRequired;'
env=os.environ.copy();env.update({'CARGO_HOME':str(Path.home()/'relay-tcp-forward-cbd3-light-v4/cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'1','NUM_JOBS':'1','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld'})
artifact=root/'artifacts';artifact.mkdir()
def build(label):
 log=root/(label+'.build.log')
 args=['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target','x86_64-unknown-linux-musl','-j','1','--message-format=json']
 with log.open('wb') as out:status=subprocess.run(args,cwd=check,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=180,check=False).returncode
 rows=[]
 for line in log.read_text().splitlines():
  try:
   row=json.loads(line)
   if row.get('reason')=='compiler-artifact' and row.get('executable') and row.get('profile',{}).get('test'):rows.append(row['executable'])
  except ValueError:pass
 if status or len(rows)!=1:
  print(log.read_text()[-6000:],flush=True);raise RuntimeError('exact native build failed')
 binary=artifact/label;shutil.copy2(rows[0],binary)
 return binary
def run_case(binary,name,kind='reset',scenario='server-first'):
 given={'parent_timeout_ms':45000,'env':{'SHARDS_TCP_SCENARIO':scenario,'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'5441494c0d0a','SHARDS_TCP_STORAGE':'caller'}}
 supplied=root/(name+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
 status=subprocess.run(['python3',str(source/'docs/research/measurements/managed-tcp-forward/implementation/native_cases.py'),'--case',kind,'--binary',str(binary),'--inputs',str(supplied),'--record',str(root/name)],timeout=60,check=False).returncode
 result=json.loads((root/name/'result.json').read_text());observed=json.loads((root/name/'observed.json').read_text())
 return {'name':name,'launcher_status':status,'result':result,'observed':observed}
result={'root':str(root),'before_shared_sha256':hashlib.sha256(exact).hexdigest(),'mutation':'remove only the new TCP peer-reset finalizer call; shared pump unchanged, owned loopback fixture only'}
try:
 shared.write_bytes(exact.replace(needle,removed));result['mutant_shared_sha256']=hashlib.sha256(shared.read_bytes()).hexdigest()
 mutant=build('mutant-musl')
 row=run_case(mutant,'mutant-server-first')
 result['mutant']=row
 reset=[r for r in row['observed'] if r.get('type')=='NativeReset']
 backend=[r for r in row['observed'] if r.get('type')=='Endpoint' and r.get('role')=='ResetBackend']
 cleanup=[r for r in row['observed'] if r.get('type')=='NativeResetCleanup']
 result['meaningful_red']=row['result']['status']==101 and len(reset)==2 and reset[0]['client_error_kind']=='Some(ConnectionReset)' and reset[1]['client_error_kind']=='None' and len(backend)==2 and all(r['read_hex']=='5245515545535400ff0d0a0102' for r in backend) and len(cleanup)==2 and all(r['fd_before']==r['fd_after'] for r in cleanup) and row['result']['reaped'] and row['result']['pidfd_exit_ready'] and row['result']['fd_before']==row['result']['fd_after']
except Exception as error:
 result['error']={'type':type(error).__name__,'detail':str(error)}
finally:
 shared.write_bytes(exact)
 result['restored_shared_sha256']=hashlib.sha256(shared.read_bytes()).hexdigest()
 after={str(p.relative_to(source)):hashlib.sha256(p.read_bytes()).hexdigest() for p in source.rglob('*') if p.is_file()}
 result['all_source_differences']=[k for k in sorted(set(before)|set(after)) if before.get(k)!=after.get(k)]
 result['named_file_diff_status']=subprocess.run(['git','diff','--no-index','--exit-code',str(root/'before-shared.rs'),str(shared)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False).returncode
 (root/'restore.json').write_text(json.dumps(result,indent=2))
 print(json.dumps({k:v for k,v in result.items() if k!='mutant'}),flush=True)
if not result.get('meaningful_red') or result['all_source_differences'] or result['named_file_diff_status']!=0:raise RuntimeError('mutation/effect/exact restoration proof failed')
restored=build('restored-musl')
controls=[run_case(restored,'restored-reset'),run_case(restored,'restored-server-first',kind='both-half-closes'),run_case(restored,'restored-backend-fin-first',kind='both-half-closes',scenario='backend-fin-first'),run_case(restored,'restored-receipt-cancel',kind='receipt-cancel')]
result['restored_controls']=controls
result['passed']=all(r['launcher_status']==0 and r['result']['passed'] for r in controls)
(root/'result.json').write_text(json.dumps(result,indent=2))
print(json.dumps({'root':str(root),'meaningful_red':result['meaningful_red'],'restored_source_sha256':result['restored_shared_sha256'],'all_source_differences':result['all_source_differences'],'controls':[{'name':r['name'],'status':r['result']['status'],'passed':r['result']['passed']} for r in controls]}),flush=True)
sys.exit(0 if result['passed'] else 1)
