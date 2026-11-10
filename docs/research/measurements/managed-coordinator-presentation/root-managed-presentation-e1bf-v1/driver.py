from pathlib import Path
import subprocess,os,json,hashlib
r=Path.home()/'root-managed-presentation-e1bf-v1';base=json.loads((Path.home()/'root-managed-sockaddr-2228-runtime-v2/connected/inputs.json').read_text());env=os.environ.copy();env.update(base)
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','--no-run','--message-format=json'];(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binary=None
for line in (r/'compile.jsonl').read_text().splitlines():
 item=json.loads(line)
 if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='managed_network' and item.get('executable'):binary=item['executable']
assert binary;(r/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n')
results=[]
for mode in ['connected','unconnected']:
 for profile in ['destination-only','fixed-original-peer']:
  name=f'{mode}-{profile}';d=r/name;d.mkdir();settings=base|{'SHARDS_SOCK_ADDR_MODE':mode,'SHARDS_SOCK_ADDR_PRESENTATION':profile,'SHARDS_NETWORK_RECORDS':str(d)};runenv=env|settings;(d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
  cmd=[binary,'direct_guest_socket_address_udp4_requires_original_native_tuple','--exact','--ignored','--nocapture','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
  with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=runenv,stdout=out,stderr=subprocess.STDOUT)
  (d/'test.exit').write_text(str(p.returncode)+'\n');path=d/f'sock-addr-udp4-{name}.stdout';records=[]
  if path.exists():
   for line in path.read_text(errors='replace').splitlines():
    if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
  (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n');clients=[i for i in records if i['type']=='ClientCompletion'];cleanup=[i for i in records if i['type']=='Cleanup' and i['state']=='Observed'];errors=[i for i in records if i['type'] in ['Error','HookError','UdpError']]
  result={'mode':mode,'profile':profile,'actual_exit':p.returncode,'clients':clients,'cleanup':cleanup,'errors':errors};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(result),flush=True)
assert all((i['actual_exit']==0) if i['profile']=='fixed-original-peer' else (i['actual_exit']!=0 and len(i['clients'])==2 and i['clients'][-1]['compatibility']=='Mismatch') for i in results),'actual profiles did not meet strict selected expectations; preserve failures'
