from pathlib import Path
import subprocess,os,json,hashlib
r=Path.home()/'root-managed-output-exit-ebbd-v1';env=os.environ.copy();cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_output','--no-run','--message-format=json'];(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binary=None
for line in (r/'compile.jsonl').read_text().splitlines():
 item=json.loads(line)
 if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='managed_output' and item.get('executable'):binary=item['executable']
assert binary;(r/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n')
cmd=[binary,'--nocapture','--test-threads=1'];(r/'regression.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'regression.log').open('wb') as out:p=subprocess.run(cmd,env=env,stdout=out,stderr=subprocess.STDOUT)
(r/'regression.exit').write_text(str(p.returncode)+'\n');print('default output regression actual exit',p.returncode,flush=True)
if p.returncode:raise SystemExit(p.returncode)
results=[]
for n in [0,1,2]:
 d=r/f'members-{n}';settings={'SHARDS_OUTPUT_COST_MEMBERS':str(n),'SHARDS_OUTPUT_COST_RECORDS':str(d),'SHARDS_OUTPUT_COST_DEADLINE_MS':'300000'};runenv=env|settings;(r/f'members-{n}.environment.json').write_text(json.dumps(settings,indent=2)+'\n');cmd=[binary,'cost::owner_output_resources_at_explicit_member_count','--exact','--ignored','--nocapture','--test-threads=1'];(r/f'members-{n}.command.json').write_text(json.dumps(cmd)+'\n')
 with (r/f'members-{n}.log').open('wb') as out:p=subprocess.run(cmd,env=runenv,stdout=out,stderr=subprocess.STDOUT)
 (r/f'members-{n}.exit').write_text(str(p.returncode)+'\n');teardown=json.loads((d/'teardown.json').read_text()) if (d/'teardown.json').exists() else None;result={'members':n,'actual_exit':p.returncode,'teardown':teardown};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(result),flush=True)
 if p.returncode:raise SystemExit(p.returncode)
