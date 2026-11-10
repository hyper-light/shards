python3 - <<'PY'
from pathlib import Path
import os,subprocess,resource,shutil,json,tomllib,hashlib,base64,gzip
root=Path.home()/'relay-engine-unix-clippy-v2'
if shutil.disk_usage(Path.home()).free<2*1024**3:raise RuntimeError('2GiB private floor')
root.mkdir(exist_ok=False)
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]});resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
original=Path.home()/'relay-tcp-forward-cbd3-light-v13'
source=root/'source';check=root/'check';shutil.copytree(original/'source',source);shutil.copytree(original/'check',check)
cpp=source/'crates/testguest/src/managed_network/intercept/tcp_forward.rs'
if hashlib.sha256(cpp.read_bytes()).hexdigest()!='a534974aba72d006f4766703df2189eb621b60b7014d9c96bdd8575879e0b2a0':raise RuntimeError('exact previous fixture source')
cpp.write_bytes(gzip.decompress(base64.b64decode(os.environ['TCP_STYLE_CPP_PACKET'])))
subprocess.run(['rustup','run','1.98.1','rustfmt','--edition','2024','--config','skip_children=true',str(cpp)],check=True)
normal=Path.home()/'relay-tcp-engine-consumer-3bbd-guest-v2/source'
shutil.copy2(normal/'clippy.toml',source/'clippy.toml')
settings=tomllib.loads((normal/'Cargo.toml').read_text())['workspace']['lints']
flags=[]
for group,items in settings.items():
 for name,value in items.items():
  level=value if isinstance(value,str) else value['level']
  flags += ['--'+level,('clippy::' if group=='clippy' else '')+name]
env=os.environ.copy();env.update({'CLIPPY_CONF_DIR':str(source),'CARGO_HOME':str(Path.home()/'relay-tcp-forward-cbd3-light-v4/cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'1','CARGO_INCREMENTAL':'0','NUM_JOBS':'1','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld'})
results=[]
for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
 command=['rustup','run','1.98.1','cargo','clippy','--lib','--all-targets','--target',target,'-j','1','--',*flags,'-D','warnings']
 log=root/(target+'.log')
 with log.open('wb') as out:status=subprocess.run(command,cwd=check,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=300,check=False).returncode
 row={'target':target,'status':status,'command':command,'log':str(log),'sha256':hashlib.sha256(log.read_bytes()).hexdigest()};results.append(row);print(json.dumps(row),flush=True)
 if status:print(log.read_text()[-16000:],flush=True)
(root/'result.json').write_text(json.dumps({'scope':'private source projection, not full testguest caller; workspace lint flags and unmodified clippy config','source_sha256':hashlib.sha256((source/'crates/testguest/src/managed_network/intercept/tcp_forward.rs').read_bytes()).hexdigest(),'workspace_lints':settings,'results':results},indent=2))

native=[]
if all(r['status']==0 for r in results):
 for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
  log=root/(target+'.test-build.log')
  with log.open('wb') as out: status=subprocess.run(['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target',target,'-j','1','--message-format=json'],cwd=check,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=180,check=False).returncode
  if status:raise RuntimeError('final exact source test build')
  artifacts=[]
  for line in log.read_text().splitlines():
   try:
    row=json.loads(line)
    if row.get('reason')=='compiler-artifact' and row.get('executable') and row.get('profile',{}).get('test'):artifacts.append(row['executable'])
   except ValueError:pass
  if len(artifacts)!=1:raise RuntimeError('one exact binary')
  for name,kind,scenario,half in [('unix-client','engine-unix','server-first','client-fin-first'),('unix-host','engine-unix','server-first','host-fin-first'),('tcp-server','both-half-closes','server-first','client-fin-first'),('tcp-backend-fin','both-half-closes','backend-fin-first','host-fin-first'),('extra-client','extra-client-byte','server-first','client-fin-first'),('cancel','receipt-cancel','server-first','client-fin-first'),('backend-reset','reset','server-first','client-fin-first'),('client-reset','client-reset','server-first','client-fin-first')]:
   case=target+'-'+name
   given={'parent_timeout_ms':45000,'env':{'SHARDS_TCP_SCENARIO':scenario,'SHARDS_ENGINE_HALF':half,'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'5441494c0d0a','SHARDS_TCP_STORAGE':'caller','SHARDS_TCP_EXTRA_HEX':'58'}}
   supplied=root/(case+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
   status=subprocess.run(['python3',str(source/'docs/research/measurements/managed-tcp-forward/implementation/native_cases.py'),'--case',kind,'--binary',artifacts[0],'--inputs',str(supplied),'--record',str(root/case)],timeout=60,check=False).returncode
   native.append({'name':case,'status':status})
   if status:break
  if native[-1]['status']:break
(root/'native-result.json').write_text(json.dumps({'source_sha256':hashlib.sha256(cpp.read_bytes()).hexdigest(),'native':native,'source':str(source),'target':str(root/'target')},indent=2))
print(json.dumps({'root':str(root),'clippy_status':[r['status'] for r in results],'native':native}),flush=True)
raise SystemExit(0 if all(r['status']==0 for r in results+native) and native else 1)
PY
