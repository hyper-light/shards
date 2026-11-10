python3 - <<'PY'
from pathlib import Path
import os,json,subprocess,hashlib,resource,shutil
original=Path.home()/'relay-engine-unix-eof-mutation-v1'
if shutil.disk_usage(original).free<2*1024**3:raise RuntimeError('2GiB private floor')
root=original/'resupplied-controls-v1';root.mkdir(exist_ok=False)
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]});resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
source=original/'source';binary=original/'artifacts/restored-musl'
expected='222483025120546df6f4a84bd344a5a892fefa8dc70c77a140733c951b0ac7d7'
if hashlib.sha256(binary.read_bytes()).hexdigest()!=expected:raise RuntimeError('frozen binary identity')
shared=source/'crates/init/src/relay.rs'
if hashlib.sha256(shared.read_bytes()).hexdigest()!='083bce5ec7f0a96c74d1e44c0aa3d882344a36fce1401b017f8e114a8841fe27':raise RuntimeError('restored source identity')
results=[]
for name,kind in [('extra-client-byte','extra-client-byte'),('backend-reset','reset'),('client-reset','client-reset'),('receipt-cancel','receipt-cancel')]:
 env={'SHARDS_TCP_SCENARIO':'server-first','SHARDS_NETWORK_CASE_MS':'30000','SHARDS_INTERCEPT_MARK':'185','SHARDS_INTERCEPT_TABLE':'185','SHARDS_INTERCEPT_RULE_PRIORITY':'185','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_GREETING_HEX':'53455256455200ff','SHARDS_TCP_TAIL_HEX':'5441494c0d0a','SHARDS_TCP_STORAGE':'caller','SHARDS_TCP_EXTRA_HEX':'58'}
 supplied=root/(name+'.inputs.json');supplied.write_text(json.dumps({'parent_timeout_ms':45000,'env':env},indent=2))
 status=subprocess.run(['python3',str(source/'docs/research/measurements/managed-tcp-forward/implementation/native_cases.py'),'--case',kind,'--binary',str(binary),'--inputs',str(supplied),'--record',str(root/name)],timeout=60,check=False).returncode
 result=json.loads((root/name/'result.json').read_text());results.append({'name':name,'launcher_status':status,'result':result})
final={'original':'r117 four nonmatching scenario controls retained; new records use required ServerFirst input','root':str(root),'binary_sha256':expected,'binary_after_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'shared_sha256':hashlib.sha256(shared.read_bytes()).hexdigest(),'results':results}
(root/'result.json').write_text(json.dumps(final,indent=2))
print(json.dumps({'root':str(root),'unchanged_binary':final['binary_after_sha256']==expected,'controls':[{'name':r['name'],'status':r['result']['status'],'passed':r['result']['passed']} for r in results]}),flush=True)
raise SystemExit(0 if all(r['launcher_status']==0 and r['result']['passed'] for r in results) and final['binary_after_sha256']==expected else 1)
PY
