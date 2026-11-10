python3 - <<'PY'
from pathlib import Path
import os,sys,subprocess,resource,hashlib,json,gzip,base64
import shutil
if shutil.disk_usage(Path.home()).free < 2*1024**3: raise RuntimeError('private 2GiB disk floor')
overrides=json.loads(gzip.decompress(base64.b64decode(os.environ['TCP_SOURCE_PACKET_A']+os.environ['TCP_SOURCE_PACKET_B']+os.environ['TCP_SOURCE_PACKET_C'])))
root=Path.home()/'relay-tcp-forward-cbd3-light-v13'
root.mkdir(exist_ok=False)
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]})
resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
source=root/'source'
for item in overrides:
 rel=Path(item['path'])
 if rel.is_absolute() or '..' in rel.parts: raise ValueError('unsafe path')
 data=item['text'].encode()
 if hashlib.sha256(data).hexdigest()!=item['sha256']: raise ValueError('override identity mismatch')
 out=source/rel; out.parent.mkdir(parents=True,exist_ok=True); out.write_bytes(data)
(root/'override-identities.json').write_text(json.dumps([{k:v for k,v in f.items() if k!='text'} for f in overrides],indent=2)+'\n')
light=root/'check'
(light/'devcgroup').mkdir(parents=True)
(light/'devcgroup/Cargo.toml').write_text('[package]\nname="shards-devcgroup"\nversion="0.1.0"\nedition="2024"\n[lib]\npath="../../source/crates/devcgroup/src/lib.rs"\n')
(light/'Cargo.toml').write_text('[package]\nname="relay-shared-check"\nversion="0.0.0"\nedition="2024"\nrust-version="1.98.1"\n[workspace]\nmembers=["devcgroup"]\n[lib]\npath="../source/crates/testguest/src/interface_root.rs"\n[dependencies]\nlibc="=0.2.189"\nshards-devcgroup={path="devcgroup"}\n')
(source/'crates/testguest/src/interface_root.rs').write_text('#![allow(dead_code)]\nmod managed_network;\n')
env=os.environ.copy()
env.update({'CARGO_HOME':str(Path.home()/'relay-tcp-forward-cbd3-light-v4/cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'1','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld','NUM_JOBS':'1'})
checks=[]
def run(name,args,cwd):
 log=root/(name+'.log')
 with log.open('w') as f: status=subprocess.run(args,cwd=cwd,env=env,stdout=f,stderr=subprocess.STDOUT,check=False).returncode
 row={'name':name,'status':status,'command':args,'log':str(log),'sha256':hashlib.sha256(log.read_bytes()).hexdigest()}
 checks.append(row);print(json.dumps(row),flush=True)
 if status:
  for line in log.read_text().splitlines():
   try:
    row=json.loads(line)
    if row.get('reason')=='compiler-message' and row['message']['level']=='error': print(row['message']['rendered'],flush=True)
   except (ValueError,KeyError):
    if 'error' in line or 'warning' in line: print(line,flush=True)
for f in overrides:
 if f['path'].endswith('.rs'):
  subprocess.run(['rustup','run','1.98.1','rustfmt','--edition','2024','--config','skip_children=true',str(source/f['path'])],cwd=source,check=True)
(root/'source-identities.json').write_text(json.dumps([{'path':str(p.relative_to(source)),'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in source.rglob('*') if p.is_file()],indent=2)+'\n')
run('policy',['python3','scripts/check-development-policy'],source)
for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
 run(target,['rustup','run','1.98.1','cargo','check','--lib','--target',target,'-j','1'],light)
 run(target+'-test-build',['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target',target,'-j','1','--message-format=json'],light)
prior=root
for log in prior.glob('*test-build.log'):
 for line in log.read_text().splitlines():
  try:
   row=json.loads(line)
   if row.get('reason')=='compiler-message' and row['message']['level']=='error': print(json.dumps({'prior_log':str(log),'rendered':row['message']['rendered']}),flush=True)
  except (ValueError,KeyError): pass
native=[]
if all(x['status']==0 for x in checks):
 for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
  artifacts=[]
  for line in (root/(target+'-test-build.log')).read_text().splitlines():
   try:
    item=json.loads(line)
    if item.get('reason')=='compiler-artifact' and item.get('executable') and item.get('profile',{}).get('test'): artifacts.append(item['executable'])
   except ValueError: pass
  if len(artifacts)!=1: raise RuntimeError('one exact source-linked test binary required')
  binary=artifacts[0]
  for storage in ['caller']:
   for scenario in ['server-first','backend-fin-first']:
    name=target+'-'+storage+'-'+scenario
    size=os.sysconf('SC_PAGESIZE')
    given={'parent_timeout_ms':45000,'env':{
     'SHARDS_TCP_SCENARIO':scenario,'SHARDS_NETWORK_CASE_MS':'30000',
     'SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185',
     'SHARDS_TCP_UP_BYTES':str(size if storage=='minimum-mapped' else 3),
     'SHARDS_TCP_DOWN_BYTES':str(size if storage=='minimum-mapped' else 5),
     'SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102',
     'SHARDS_TCP_GREETING_HEX':'53455256455200ff',
     'SHARDS_TCP_TAIL_HEX':'5441494c0d0a',
     'SHARDS_TCP_STORAGE':storage,'SHARDS_TCP_EXTRA_HEX':'58','SHARDS_ENGINE_HALF':('client-fin-first' if scenario=='server-first' else 'host-fin-first')}}
    supplied=root/(name+'.inputs.json'); supplied.write_text(json.dumps(given,indent=2))
    status=subprocess.run(['python3',str(source/'docs/research/measurements/managed-tcp-forward/implementation/native_cases.py'),'--case','engine-unix','--binary',binary,'--inputs',str(supplied),'--record',str(root/name)],timeout=60,check=False).returncode
    native.append({'name':name,'status':status})
    if status: break
   if native[-1]['status']: break
  if native[-1]['status']: break
result={'root':str(root),'checks':checks,'native':native,'native_execution':bool(native)}
(root/'result.json').write_text(json.dumps(result,indent=2))
print(json.dumps(result),flush=True)
sys.exit(0 if all(x['status']==0 for x in checks+native) else 1)
PY
