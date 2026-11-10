from pathlib import Path
import os,json,subprocess,hashlib,shutil,time,difflib
h=Path.home();r=h/'root-managed-pending-2b7d-v1';r.mkdir(exist_ok=False);(r/'driver.py').write_text(Path(__file__).read_text())
source='2b7db81cb19f1f0f464d7c5e51518177dbbb6889';file=Path('crates/testguest/src/managed_network.rs');original=file.read_bytes()
(r/'source.txt').write_text(source+'\n');(r/'original-source.sha256').write_text(hashlib.sha256(original).hexdigest()+'\n')
base=json.loads((h/'root-managed-admission-c141-v1/forward-connected/environment.json').read_text());base.pop('SHARDS_NETWORK_RECORDS',None)
env=os.environ.copy()|base
for k in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(k,None)
env.update(CARGO_BUILD_JOBS='4',CARGO_TARGET_DIR=str(h/'managed-capture-check/target'),SHARDS_REQUIRE_VMS='1')
results=[];frozen=h/'root-managed-pending-2b7d-artifacts-v1';frozen.mkdir()
def build(world):
 d=r/world;d.mkdir()
 revision=source if world!='precheck-removed' else source+'+pending-precheck-removed'
 e=env|{'SHARDS_NETWORK_SOURCE_REVISION':revision};cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','--no-run','--message-format=json']
 (d/'compile.command.json').write_text(json.dumps(cmd)+'\n')
 with (d/'compile.jsonl').open('wb') as out,(d/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=e,stdout=out,stderr=err)
 (d/'compile.exit').write_text(str(p.returncode)+'\n')
 assert p.returncode==0
 binary=None
 for line in (d/'compile.jsonl').read_text().splitlines():
  v=json.loads(line)
  if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_network' and v.get('executable'):binary=v['executable']
 assert binary;(d/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest(),'actual_guest_source_sha256':hashlib.sha256(file.read_bytes()).hexdigest()},indent=2)+'\n')
 return d,binary,e
def case(world,binary,e,name,test,mode,expected):
 d=world/name;d.mkdir();settings=base|{'SHARDS_SOCK_ADDR_MODE':mode,'SHARDS_SOCK_ADDR_PRESENTATION':'fixed-original-peer','SHARDS_SOCK_ADDR_BUFFER_BYTES':'18','SHARDS_NETWORK_RECORDS':str(d)};(d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 cmd=[binary,test,'--exact','--ignored','--nocapture','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
 with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=e|settings,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(p.returncode)+'\n');records=[]
 for path in d.glob('*.stdout'):
  for line in path.read_text(errors='replace').splitlines():
   if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
 (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
 for ip in d.glob('*.identity.json'):
  guest=json.loads(ip.read_text())['guest'];at=Path(guest['path']);digest=guest['sha256'].removeprefix('sha256:');assert hashlib.sha256(at.read_bytes()).hexdigest()==digest
  target=frozen/(digest+'.guest')
  if not target.exists():shutil.copy2(at,target)
 result={'world':world.name,'case':name,'mode':mode,'actual_exit':p.returncode,'expected_exit':expected,'forward':[v for v in records if v['type']=='ForwardCompletion'],'backend':[v for v in records if v['type']=='UdpArrival' and v.get('role')=='Backend'],'cleanup':[v for v in records if v['type']=='Cleanup' and v['state']=='Observed']};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(world.name,name,p.returncode,flush=True)
 assert p.returncode==expected,result
 assert len(result['cleanup'])==1 and all(result['cleanup'][0][k] for k in ['backend_joined','relay_joined','link_detached','cgroup_removed'])
 if test.endswith('requires_no_backend_effect'):
  assert len(result['backend'])==(3 if expected else 2)
  assert result['forward'][0]['upstream_sent']==(1 if expected else 0)
pre='cancel::direct_guest_udp4_precommit_cancel_requires_no_backend_effect'
try:
 d,b,e=build('corrected')
 for mode in ['connected','unconnected']:case(d,b,e,'precommit-'+mode,pre,mode,0)
 s=original.decode();a=s.index('            if let Some(cancel) = self.cancel {',s.index('    fn io<T>'));b=s.index('            match operation() {',a);removed=s[:a]+s[b:]
 (r/'mutation.patch').write_text(''.join(difflib.unified_diff(s.splitlines(True),removed.splitlines(True),fromfile=str(file),tofile=str(file))))
 file.write_text(removed);os.utime(file,ns=(time.time_ns(),time.time_ns()))
 d,b,e=build('precheck-removed');case(d,b,e,'precommit-connected',pre,'connected',101)
finally:
 file.write_bytes(original);os.utime(file,ns=(time.time_ns(),time.time_ns()))
 assert file.read_bytes()==original
 (r/'restored-source.sha256').write_text(hashlib.sha256(file.read_bytes()).hexdigest()+'\n')
 diff=subprocess.check_output(['git','diff','--',str(file)]);(r/'restored.diff').write_bytes(diff);assert not diff
d,b,e=build('restored')
for mode in ['connected','unconnected']:case(d,b,e,'precommit-'+mode,pre,mode,0)
for kind,test in [('receive','cancel::direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline'),('forward','direct_guest_two_leg_udp4_records_buffers_and_backend_source_change'),('scripted','direct_guest_socket_address_udp4_requires_original_native_tuple')]:
 for mode in ['connected','unconnected']:case(d,b,e,kind+'-'+mode,test,mode,0)
(r/'driver.exit').write_text('0\n')
