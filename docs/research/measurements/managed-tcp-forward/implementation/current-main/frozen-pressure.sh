python3 - <<'PY'
from pathlib import Path
import os,resource,json,subprocess,hashlib,shutil
home=Path.home();root=home/'relay-tcp-corrected-pressure-v1'
if shutil.disk_usage(home).free<2*1024**3:raise RuntimeError('2GiB private floor')
root.mkdir(exist_ok=False)
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]});resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
source=home/'relay-engine-unix-clippy-v3/source';driver=source/'docs/research/measurements/managed-tcp-forward/implementation/native_cases.py'
shared=source/'crates/init/src/relay.rs'
if hashlib.sha256(shared.read_bytes()).hexdigest()!='083bce5ec7f0a96c74d1e44c0aa3d882344a36fce1401b017f8e114a8841fe27':raise RuntimeError('source identity')
identities=[('x86_64-unknown-linux-musl','e2eb1fe659e1a9845041001b2102cec886f9827052fb0d1c0df4e4612ae15bd1'),('x86_64-unknown-linux-gnu','d251660f2ae1838b5b0b7d68610ab9d3bd6cd9275b9a2710aef1aac834a505fa')]
results=[]
for target,expected in identities:
 binary=home/'relay-engine-unix-clippy-v3/artifacts'/target
 if hashlib.sha256(binary.read_bytes()).hexdigest()!=expected:raise RuntimeError('frozen native binary')
 for scenario in ['server-first','backend-fin-first']:
  name=target+'-'+scenario
  given={'parent_timeout_ms':45000,'env':{'SHARDS_TCP_SCENARIO':scenario,'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'65536','SHARDS_TCP_DOWN_BYTES':'65536','SHARDS_TCP_REQUEST_HEX':'repeat:1048576:00ff0d0a01020304','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'repeat:131072:5441494c00ff','SHARDS_TCP_STORAGE':'caller','SHARDS_NATIVE_WRITE_PRESSURE':'required','SHARDS_NATIVE_SNDBUF_BYTES':'4096','SHARDS_NATIVE_RCVBUF_BYTES':'4096'}}
  supplied=root/(name+'.inputs.json');supplied.write_text(json.dumps(given,indent=2))
  status=subprocess.run(['python3',str(driver),'--case','both-half-closes','--binary',str(binary),'--inputs',str(supplied),'--record',str(root/name)],timeout=60,check=False).returncode
  observed=json.loads((root/name/'observed.json').read_text());flow=next((r for r in observed if r.get('type')=='NativeFlow'),None)
  endpoints=[]
  for row in observed:
   if row.get('type')=='Endpoint':
    payload=bytes.fromhex(row['read_hex'])
    endpoints.append({k:v for k,v in row.items() if k!='read_hex'}|{'read_bytes':len(payload),'read_sha256':hashlib.sha256(payload).hexdigest()})
  result=json.loads((root/name/'result.json').read_text())
  results.append({'case':name,'launcher_status':status,'result':result,'flow':flow,'endpoints':endpoints,'queues':[r for r in observed if r.get('type')=='NativeQueue']})
  if status:break
 if results[-1]['launcher_status']:break
final={'source_commit':'37ee895ed27b7e5e91f061f4ab2496229b24781b','shared_sha256':hashlib.sha256(shared.read_bytes()).hexdigest(),'fixture_sha256':hashlib.sha256((source/'crates/testguest/src/managed_network/intercept/tcp_forward.rs').read_bytes()).hexdigest(),'root':str(root),'cases':results,'raw_scope':'full original stdout/observed remain private remote; compact endpoint hashes retain actual bytes identity'}
(root/'summary.json').write_text(json.dumps(final,indent=2))
print(json.dumps({'root':str(root),'cases':[{'case':r['case'],'status':r['result']['status'],'passed':r['result']['passed'],'flow':r['flow']} for r in results]}),flush=True)
raise SystemExit(0 if len(results)==4 and all(r['launcher_status']==0 and r['result']['passed'] for r in results) else 1)
PY
