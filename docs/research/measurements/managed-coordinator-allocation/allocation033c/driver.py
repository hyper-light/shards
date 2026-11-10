from pathlib import Path
import os,json,subprocess,hashlib
h=Path.home();r=h/'root-managed-allocation-033c-v1';r.mkdir(exist_ok=False);(r/'driver.py').write_text(Path(__file__).read_text())
source='033c621c4ad3066460dfc1c2c1edf4a094b86ef2';(r/'source.txt').write_text(source+'\n')
env=os.environ.copy()
for k in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(k,None)
kernel=str(h/'managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy')
settings={'CARGO_BUILD_JOBS':'4','CARGO_TARGET_DIR':str(h/'managed-capture-check/target'),'SHARDS_REQUIRE_VMS':'1','SHARDS_KERNEL':kernel,'SHARDS_TEST_KERNEL':kernel,'SHARDS_OUTPUT_COST_SOURCE_REVISION':source}
env.update(settings);(r/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_output','--no-run','--message-format=json'];(r/'compile.command.json').write_text(json.dumps(cmd)+'\n')
with (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
(r/'compile.exit').write_text(str(p.returncode)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binary=None
for line in (r/'compile.jsonl').read_text().splitlines():
 v=json.loads(line)
 if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable'):binary=v['executable']
assert binary;(r/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n')
cases=[('ordinary',None)]+[(f'members-{n}',n) for n in [0,1,2]];results=[]
for name,n in cases:
 d=r/name;d.mkdir();extra={}
 cmd=[binary,'--nocapture','--test-threads=1'] if n is None else [binary,'cost::owner_output_resources_at_explicit_member_count','--exact','--ignored','--nocapture','--test-threads=1']
 if n is not None:extra={'SHARDS_OUTPUT_COST_MEMBERS':str(n),'SHARDS_OUTPUT_COST_DEADLINE_MS':'300000','SHARDS_OUTPUT_COST_RECORDS':str(d/'records')}
 (d/'command.json').write_text(json.dumps(cmd)+'\n');(d/'environment.json').write_text(json.dumps(extra,indent=2)+'\n')
 with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=env|extra,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(p.returncode)+'\n');results.append({'case':name,'members':n,'exit':p.returncode});(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(name,p.returncode,flush=True)
 if p.returncode:raise SystemExit(p.returncode)
(r/'driver.exit').write_text('0\n')
