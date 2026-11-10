from pathlib import Path
import subprocess,os,json,hashlib,shutil
r=Path.home()/'root-managed-admission-c141-v1';r.mkdir()
old=Path.home()/'root-managed-forward-guards-7ee6-v2'
assert (old/'driver.actual-exit').read_text().strip()=='0'
assert (old/'restored.diff').read_bytes()==b''
for name,digest in json.loads((old/'restored-source.json').read_text()).items():assert hashlib.sha256(Path(name).read_bytes()).hexdigest()==digest
assert subprocess.check_output(['git','status','--porcelain'])==b''
(r/'previous-restoration.json').write_bytes((old/'restored-source.json').read_bytes())
with (r/'fetch.log').open('wb') as out:subprocess.run(['git','fetch','origin','p/managed-network-admit-first'],stdout=out,stderr=subprocess.STDOUT,check=True)
with (r/'checkout.log').open('wb') as out:subprocess.run(['git','checkout','--detach','c141f67d01777b93ab0b41f8628a68c9f911a370'],stdout=out,stderr=subprocess.STDOUT,check=True)
(r/'source.txt').write_bytes(subprocess.check_output(['git','rev-parse','HEAD']))
driver=Path(__file__);shutil.copy2(driver,r/'driver.py');(r/'driver.sha256').write_text(hashlib.sha256(driver.read_bytes()).hexdigest()+'\n')
base=json.loads((Path.home()/'root-managed-forward-7ee6-v1/connected-fixed-original-peer/environment.json').read_text());env=os.environ.copy()|base
env['CARGO_TARGET_DIR']=str(Path.home()/'managed-capture-check/target');env['CARGO_BUILD_JOBS']='4';env.pop('SHARDS_HELPERS',None)
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','--no-run','--message-format=json']
(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binary=None
for line in (r/'compile.jsonl').read_text().splitlines():
 v=json.loads(line)
 if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_network' and v.get('executable'):binary=v['executable']
assert binary
(r/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n')
frozen=Path.home()/'root-managed-admission-c141-artifacts-v1';frozen.mkdir();results=[]
for operation,budget in [('refusal',17),('forward',18),('scripted',18)]:
 for mode in ['connected','unconnected']:
  d=r/f'{operation}-{mode}';d.mkdir();settings=base|{'SHARDS_SOCK_ADDR_MODE':mode,'SHARDS_SOCK_ADDR_BUFFER_BYTES':str(budget),'SHARDS_NETWORK_RECORDS':str(d)}
  (d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
  name='direct_guest_socket_address_udp4_requires_original_native_tuple' if operation=='scripted' else 'direct_guest_two_leg_udp4_records_buffers_and_backend_source_change'
  prefix=f"sock-addr-{'udp4' if operation=='scripted' else 'relay4'}-{mode}-fixed-original-peer"
  cmd=[binary,name,'--exact','--ignored','--nocapture','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
  with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=env|settings,stdout=out,stderr=subprocess.STDOUT)
  (d/'test.exit').write_text(str(p.returncode)+'\n')
  records=[]
  for line in (d/(prefix+'.stdout')).read_text(errors='replace').splitlines():
   if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
  (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
  identity=json.loads((d/(prefix+'.identity.json')).read_text())['guest'];digest=identity['sha256'].removeprefix('sha256:');actual=Path(identity['path']);assert hashlib.sha256(actual.read_bytes()).hexdigest()==digest
  target=frozen/(digest+'.guest')
  if not target.exists():shutil.copy2(actual,target)
  cleanup=[v for v in records if v['type']=='Cleanup' and v['state']=='Observed']
  if operation=='refusal':
   assert p.returncode!=0
   refusal=[v for v in records if v['type']=='PayloadRefusal'];assert len(refusal)==1 and refusal[0]['provided']==17 and refusal[0]['required']==18 and refusal[0]['cause']=='Limit'
   assert not any(v['type'] in ['Target','ForwardTarget','Child','LinkAttached','ClientIdentity','PayloadSlots'] for v in records)
   witness={'refusal':refusal,'setup_target_absent':True,'scope':'typed return precedes setup in exact source; no general kernel-resource accounting claim'}
  else:
   assert p.returncode==0 and len(cleanup)==1 and all(cleanup[0][k] for k in ['backend_joined','relay_joined','link_detached','cgroup_removed'])
   if operation=='forward':
    slots=next(i for i,v in enumerate(records) if v['type']=='PayloadSlots');target_index=next(i for i,v in enumerate(records) if v['type']=='Target');assert slots<target_index
    witness={'slots_before_target':True,'strict_two_leg_and_backend_peer':True}
   else:
    assert not any(v['type'] in ['PayloadSlots','PayloadRefusal','ForwardTarget','ForwardExchange'] for v in records)
    witness={'scripted_exact_and_no_payload_slot_record':True}
  result={'operation':operation,'mode':mode,'budget':budget,'actual_exit':p.returncode,'witness':witness,'cleanup':cleanup,'guest_sha256':digest};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n')
  print(operation,mode,p.returncode,'actual witness checked',flush=True)
assert subprocess.check_output(['git','status','--porcelain'])==b''
(r/'driver.exit').write_text('0\n')
