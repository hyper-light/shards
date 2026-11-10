from pathlib import Path
import os,json,subprocess,hashlib,socket,shutil
home=Path.home();r=home/'root-managed-closing-1533-v1';r.mkdir(exist_ok=False)
(r/'driver.py').write_text(Path(__file__).read_text())
source='15330d9bd34cde2649e0a36f8e44f76fee520c2c'
(r/'source.txt').write_text(source+'\n')
base=json.loads((home/'root-managed-admission-c141-v1/forward-connected/environment.json').read_text())
base.pop('SHARDS_NETWORK_RECORDS',None)
env=os.environ.copy()|base
for key in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:
 env.pop(key,None)
env.update(CARGO_BUILD_JOBS='4',CARGO_TARGET_DIR=str(home/'managed-capture-check/target'),SHARDS_REQUIRE_VMS='1',SHARDS_NETWORK_SOURCE_REVISION=source,SHARDS_TENANT_EXIT_SOURCE_REVISION=source)
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','--test','managed_output','--no-run','--message-format=json']
(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binaries={}
for line in (r/'compile.jsonl').read_text().splitlines():
 v=json.loads(line)
 if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name') in ['managed_network','managed_output'] and v.get('executable'):binaries[v['target']['name']]=v['executable']
assert len(binaries)==2
(r/'test-artifacts.json').write_text(json.dumps({k:{'path':v,'sha256':hashlib.sha256(Path(v).read_bytes()).hexdigest()} for k,v in binaries.items()},indent=2)+'\n')
results=[]
cases=[('precommit-connected','managed_network','cancel::direct_guest_udp4_precommit_cancel_requires_no_backend_effect',{'SHARDS_SOCK_ADDR_MODE':'connected'}),('precommit-unconnected','managed_network','cancel::direct_guest_udp4_precommit_cancel_requires_no_backend_effect',{'SHARDS_SOCK_ADDR_MODE':'unconnected'}),('receive-connected','managed_network','cancel::direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline',{'SHARDS_SOCK_ADDR_MODE':'connected'}),('receive-unconnected','managed_network','cancel::direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline',{'SHARDS_SOCK_ADDR_MODE':'unconnected'}),('ordinary-output','managed_output',None,{})]
for name,kind,test,extra in cases:
 d=r/name;d.mkdir()
 settings=base|extra|{'SHARDS_SOCK_ADDR_BUFFER_BYTES':'18','SHARDS_SOCK_ADDR_PRESENTATION':'fixed-original-peer','SHARDS_NETWORK_RECORDS':str(d)}
 if kind=='managed_output' and test is not None:
  with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
  settings.update(SHARDS_TENANT_EXIT_PORT=str(port),SHARDS_TENANT_EXIT_DEADLINE_MS='300000',SHARDS_TENANT_EXIT_RECORDS=str(d/'records'))
 (d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 cmd=[binaries[kind],test,'--exact','--ignored','--nocapture','--test-threads=1'] if test else [binaries[kind],'--nocapture','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
 with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=env|settings,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(p.returncode)+'\n')
 result={'case':name,'test':test,'actual_exit':p.returncode}
 if kind=='managed_network':
  lines=list(d.glob('*.stdout'));records=[]
  for path in lines:
   for line in path.read_text(errors='replace').splitlines():
    if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
  (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
  result['witnesses']=[v for v in records if v['type'] in ['ForwardCompletion','UdpArrival','CancelBoundary','Cleanup','Error','ForwardError']]
 else:
  q=d/'records/teardown.json'
  result['teardown']=json.loads(q.read_text()) if q.exists() else None
 results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(name,p.returncode,'actual logs retained',flush=True)
assert all(x['actual_exit']==0 for x in results),results
(r/'driver.exit').write_text('0\n')
