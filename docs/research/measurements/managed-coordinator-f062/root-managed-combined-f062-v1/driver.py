from pathlib import Path
import os,subprocess,json,hashlib,sys
r=Path.home()/'root-managed-combined-f062-v1';env=os.environ.copy()
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','--test','managed_output','--no-run','--message-format=json']
(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:print('full caller compile failed',p.returncode,flush=True);sys.exit(p.returncode)
bins={}
for line in (r/'compile.jsonl').read_text().splitlines():
 item=json.loads(line)
 if item.get('reason')=='compiler-artifact' and item.get('executable') and item.get('target',{}).get('name') in ['managed_network','managed_output']:
  bins[item['target']['name']]=item['executable']
assert len(bins)==2
(r/'test-artifacts.json').write_text(json.dumps({k:{'path':v,'sha256':hashlib.sha256(Path(v).read_bytes()).hexdigest()} for k,v in bins.items()},indent=2)+'\n')
print('actual full callers compiled',flush=True)
for mode in ['connected','unconnected']:
 dest=r/('network-'+mode);dest.mkdir()
 settings=json.loads((Path.home()/'root-managed-sockaddr-2228-runtime-v2'/mode/'inputs.json').read_text());settings['SHARDS_NETWORK_RECORDS']=str(dest);runenv=env|settings
 (dest/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 cmd=[bins['managed_network'],'direct_guest_socket_address_udp4_requires_original_native_tuple','--exact','--ignored','--nocapture','--test-threads=1']
 (dest/'command.json').write_text(json.dumps(cmd)+'\n')
 with (dest/'test.log').open('wb') as out:p=subprocess.run(cmd,env=runenv,stdout=out,stderr=subprocess.STDOUT)
 (dest/'test.exit').write_text(str(p.returncode)+'\n')
 lines=(dest/f'sock-addr-udp4-{mode}.stdout').read_text(errors='replace').splitlines();records=[]
 for line in lines:
  if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
 (dest/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
 cleanup=[i for i in records if i['type']=='Cleanup' and i['state']=='Observed'];clients=[i for i in records if i['type']=='ClientCompletion'];reaped=[i for i in records if i['type']=='ChildReaped']
 assert p.returncode!=0,'native endpoint compatibility must not be fabricated'
 assert len(cleanup)==1 and all(cleanup[0][key] for key in ['backend_joined','relay_joined','link_detached','cgroup_removed']),cleanup
 assert len(clients)==2 and clients[0]['compatibility']=='Exact' and clients[1]['compatibility']=='Mismatch',clients
 assert len(reaped)==2 and all(i['reaped'] for i in reaped),reaped
 print(mode,'cleanup observed complete; strict endpoint compatibility remains red; actual exit',p.returncode,flush=True)
cmd=[bins['managed_output'],'--nocapture','--test-threads=1'];(r/'output-regression.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'output-regression.log').open('wb') as out:p=subprocess.run(cmd,env=env,stdout=out,stderr=subprocess.STDOUT)
(r/'output-regression.exit').write_text(str(p.returncode)+'\n');print('output regression actual exit',p.returncode,flush=True)
if p.returncode:sys.exit(p.returncode)
results=[]
for members in [0,1,2]:
 dest=r/f'cost-members-{members}';settings={'SHARDS_OUTPUT_COST_MEMBERS':str(members),'SHARDS_OUTPUT_COST_RECORDS':str(dest),'SHARDS_OUTPUT_COST_DEADLINE_MS':'300000'};runenv=env|settings
 (r/f'cost-members-{members}.environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 cmd=[bins['managed_output'],'cost::owner_output_resources_at_explicit_member_count','--exact','--ignored','--nocapture','--test-threads=1'];(r/f'cost-members-{members}.command.json').write_text(json.dumps(cmd)+'\n')
 with (r/f'cost-members-{members}.log').open('wb') as out:p=subprocess.run(cmd,env=runenv,stdout=out,stderr=subprocess.STDOUT)
 (r/f'cost-members-{members}.exit').write_text(str(p.returncode)+'\n');results.append({'members':members,'test_exit':p.returncode,'records':str(dest)})
 print('explicit member count',members,'actual exit',p.returncode,flush=True)
 (r/'cost-results.json').write_text(json.dumps(results,indent=2)+'\n')
 if p.returncode:sys.exit(p.returncode)
