from pathlib import Path
import os,sys,subprocess,resource,shutil,json,hashlib
root=Path.home()/'relay-tcp-byte-mutation-v1';root.mkdir(exist_ok=False)
if shutil.disk_usage(root).free < 2*1024**3: raise RuntimeError('private disk floor')
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]});resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
original=Path.home()/'relay-tcp-forward-cbd3-light-v7'
source=root/'source';check=root/'check';shutil.copytree(original/'source',source);shutil.copytree(original/'check',check)
before={str(p.relative_to(source)):hashlib.sha256(p.read_bytes()).hexdigest() for p in source.rglob('*') if p.is_file()}
shared=source/'crates/init/src/relay.rs';exact=shared.read_bytes()
(root/'before-shared.rs').write_bytes(exact)
needle=b'                increment(&mut progress.read, n)?;\n                progress.pending = n;'
if exact.count(needle)!=1:raise RuntimeError('one exact mutation seam required')
insert=b'                if self.source == Leg::Client && progress.read == 0 {\n                    if let Some(byte) = self.bytes.first_mut() { *byte ^= 1; }\n                }\n'
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
def run_case(binary,name,kind='both-half-closes',scenario='server-first'):
 given={'parent_timeout_ms':45000,'env':{'SHARDS_TCP_SCENARIO':scenario,'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'5441494c0d0a','SHARDS_TCP_STORAGE':'caller'}}
 supplied=root/(name+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
 status=subprocess.run(['python3',str(source/'docs/research/measurements/managed-tcp-forward/implementation/native_cases.py'),'--case',kind,'--binary',str(binary),'--inputs',str(supplied),'--record',str(root/name)],timeout=60,check=False).returncode
 result=json.loads((root/name/'result.json').read_text());observed=json.loads((root/name/'observed.json').read_text())
 return {'name':name,'launcher_status':status,'result':result,'observed':observed}
result={'root':str(root),'before_shared_sha256':hashlib.sha256(exact).hexdigest(),'mutation':'flip only the first actually consumed client payload byte before its actual upstream send; owned loopback fixture only'}
try:
 shared.write_bytes(exact.replace(needle,insert+needle));result['mutant_shared_sha256']=hashlib.sha256(shared.read_bytes()).hexdigest()
 mutant=build('mutant-musl')
 row=run_case(mutant,'mutant-server-first')
 result['mutant']=row
 backend=[r for r in row['observed'] if r.get('type')=='Endpoint' and r.get('role')=='NativeBackend']
 joined=[r for r in row['observed'] if r.get('type')=='NativeJoined']
 required=bytes.fromhex('5245515545535400ff0d0a0102');bad=bytes([required[0]^1])+required[1:]
 result['meaningful_red']=row['result']['status']==101 and len(backend)==1 and backend[0]['read_hex']==bad.hex() and not backend[0]['ok'] and len(joined)==1 and joined[0]['relay_joined'] and joined[0]['backend_joined'] and row['result']['reaped'] and row['result']['pidfd_exit_ready'] and row['result']['fd_before']==row['result']['fd_after']
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
controls=[run_case(restored,'restored-server-first'),run_case(restored,'restored-backend-fin-first',scenario='backend-fin-first'),run_case(restored,'restored-receipt-cancel',kind='receipt-cancel')]
result['restored_controls']=controls
result['passed']=all(r['launcher_status']==0 and r['result']['passed'] for r in controls)
(root/'result.json').write_text(json.dumps(result,indent=2))
print(json.dumps({'root':str(root),'meaningful_red':result['meaningful_red'],'restored_source_sha256':result['restored_shared_sha256'],'all_source_differences':result['all_source_differences'],'controls':[{'name':r['name'],'status':r['result']['status'],'passed':r['result']['passed']} for r in controls]}),flush=True)
sys.exit(0 if result['passed'] else 1)
