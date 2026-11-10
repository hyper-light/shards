from pathlib import Path
import os,json,subprocess,hashlib,shutil
r=Path.home()/'root-managed-receive-cancel-6836-v1';base=json.loads((Path.home()/'root-managed-admission-c141-v1/forward-connected/environment.json').read_text());env=os.environ.copy()|base
env['CARGO_BUILD_JOBS']='4';env['CARGO_TARGET_DIR']=str(Path.home()/'managed-capture-check/target');env['SHARDS_NETWORK_SOURCE_REVISION']='68362c3eb236f2d06e9a78984521eeb67f71ede4';env.pop('SHARDS_HELPERS',None)
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','--no-run','--message-format=json'];(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binary=None
for line in (r/'compile.jsonl').read_text().splitlines():
 v=json.loads(line)
 if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_network' and v.get('executable'):binary=v['executable']
assert binary;(r/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n')
frozen=Path.home()/'root-managed-receive-cancel-6836-artifacts-v1';frozen.mkdir();results=[]
cases=[]
for kind in ['scripted','forward']:
 for mode in ['connected','unconnected']:
  for profile in ['destination-only','fixed-original-peer']:cases.append((kind,mode,profile))
for mode in ['connected','unconnected']:cases.append(('cancel',mode,'fixed-original-peer'))
for kind,mode,profile in cases:
 d=r/f'{kind}-{mode}-{profile}';d.mkdir();settings=base|{'SHARDS_SOCK_ADDR_MODE':mode,'SHARDS_SOCK_ADDR_PRESENTATION':profile,'SHARDS_SOCK_ADDR_BUFFER_BYTES':'18','SHARDS_NETWORK_RECORDS':str(d)};(d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 name={'scripted':'direct_guest_socket_address_udp4_requires_original_native_tuple','forward':'direct_guest_two_leg_udp4_records_buffers_and_backend_source_change','cancel':'cancel::direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline'}[kind]
 cmd=[binary,name,'--exact','--ignored','--nocapture','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
 with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=env|settings,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(p.returncode)+'\n');prefix=f'sock-addr-cancel4-{mode}' if kind=='cancel' else f"sock-addr-{'udp4' if kind=='scripted' else 'relay4'}-{mode}-{profile}"
 records=[]
 path=d/(prefix+'.stdout')
 if path.exists():
  for line in path.read_text(errors='replace').splitlines():
   if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
 (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
 ident=d/(prefix+'.identity.json')
 if ident.exists():
  guest=json.loads(ident.read_text())['guest'];digest=guest['sha256'].removeprefix('sha256:');at=Path(guest['path']);assert hashlib.sha256(at.read_bytes()).hexdigest()==digest;kept=frozen/(digest+'.guest')
  if not kept.exists():shutil.copy2(at,kept)
 cleanup=[v for v in records if v['type']=='Cleanup' and v['state']=='Observed'];clients=[v for v in records if v['type']=='ClientCompletion'];forward=[v for v in records if v['type']=='ForwardCompletion'];boundaries=[v for v in records if v['type']=='CancelBoundary'];errors=[v for v in records if v['type'] in ['Error','HookError','ForwardError','UdpError']]
 result={'kind':kind,'mode':mode,'profile':profile,'actual_exit':p.returncode,'cleanup':cleanup,'clients':clients,'forward':forward,'cancel_boundaries':boundaries,'errors':errors};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(kind,mode,profile,p.returncode,'raw retained',flush=True)
 if profile=='destination-only':assert p.returncode!=0 and clients[-1]['compatibility']=='Mismatch'
 else:assert p.returncode==0,(kind,mode,result)
 assert len(cleanup)==1 and all(cleanup[0][k] for k in ['backend_joined','relay_joined','link_detached','cgroup_removed'])
(r/'driver.exit').write_text('0\n')
