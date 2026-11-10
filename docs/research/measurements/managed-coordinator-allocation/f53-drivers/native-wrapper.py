from pathlib import Path
import hashlib,json,shutil,subprocess,os
h=Path.home();root=h/'managed-capture-check'
assert not subprocess.check_output(['git','-C',str(root),'status','--porcelain'],text=True).strip()
# Preserve the completed red source's available executables before cache reuse.
old=h/'root-managed-normal-eaf0-v1';frozen=h/'root-managed-normal-eaf0-artifacts-v1';frozen.mkdir(exist_ok=False)
items=[]
for name,v in json.loads((old/'test-artifacts.json').read_text()).items():items.append((name,v['path'],v['sha256']))
inputs=json.loads((old/'normal-tenant-exit/records/inputs.json').read_text())
for name in ('cli','init','testguest'):
 v=inputs[name];items.append((name,v['path'],v['sha256'].removeprefix('sha256:')))
rows=[]
for name,path,sha in items:
 p=Path(path)
 if not p.exists():rows.append({'role':name,'source':path,'expected_hash':sha,'state':'UnavailableBeforeReuse'});continue
 data=p.read_bytes();assert hashlib.sha256(data).hexdigest()==sha
 out=frozen/name;shutil.copy2(p,out);assert hashlib.sha256(out.read_bytes()).hexdigest()==sha
 rows.append({'role':name,'source':path,'path':str(out),'sha256':sha,'state':'Frozen'})
(frozen/'MANIFEST.json').write_text(json.dumps(rows,indent=2)+'\n')
subprocess.run(['git','-C',str(root),'fetch','origin','refs/heads/main:refs/remotes/origin/main','refs/heads/p/managed-combined:refs/remotes/origin/p/managed-combined'],check=True)
rules=subprocess.check_output(['git','-C',str(root),'show','origin/main:docs/development-rules.md']);agents=subprocess.check_output(['git','-C',str(root),'show','origin/main:AGENTS.md'])
assert hashlib.sha256(rules).hexdigest()=='d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242'
subprocess.run(['git','-C',str(root),'switch','--detach','f53f7750eb6c74ce6f7de290f6977c08a401f774'],check=True)
p=h/'root-native-f53f.executed-v1.py';assert not p.exists();p.write_text("from pathlib import Path\nimport os,json,subprocess,hashlib\nh=Path.home();r=h/'root-managed-native-f53f-v1';r.mkdir(exist_ok=False);(r/'driver.py').write_text(Path(__file__).read_text())\nsource='f53f7750eb6c74ce6f7de290f6977c08a401f774';(r/'source.txt').write_text(source+'\\n')\nenv=os.environ.copy()\nfor k in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(k,None)\nkernel=str(h/'managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy')\nsettings={'CARGO_BUILD_JOBS':'4','CARGO_TARGET_DIR':str(h/'managed-capture-check/target'),'SHARDS_REQUIRE_VMS':'1','SHARDS_KERNEL':kernel,'SHARDS_TEST_KERNEL':kernel,'SHARDS_OUTPUT_COST_SOURCE_REVISION':source}\n(r/'empty-docker').mkdir();settings['DOCKER_CONFIG']=str(r/'empty-docker')\nenv.update(settings);(r/'environment.json').write_text(json.dumps(settings,indent=2)+'\\n')\nfor label,args in [('rustc',['rustc','--version','--verbose']),('cargo',['cargo','--version'])]:\n (r/(label+'.txt')).write_text(subprocess.check_output(args,text=True))\ncmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_output','--no-run','--message-format=json'];(r/'compile.command.json').write_text(json.dumps(cmd)+'\\n')\nwith (r/'compile.jsonl').open('wb') as out,(r/'compile.log').open('wb') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)\n(r/'compile.exit').write_text(str(p.returncode)+'\\n')\nif p.returncode:raise SystemExit(p.returncode)\nbinary=None\nfor line in (r/'compile.jsonl').read_text().splitlines():\n v=json.loads(line)\n if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable'):binary=v['executable']\nassert binary;(r/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\\n')\ncases=[('ordinary',None)]+[(f'members-{n}',n) for n in [1]];results=[]\nfor name,n in cases:\n d=r/name;d.mkdir();extra={}\n cmd=[binary,'--nocapture','--test-threads=1'] if n is None else [binary,'cost::owner_output_resources_at_explicit_member_count','--exact','--ignored','--nocapture','--test-threads=1']\n if n is not None:extra={'SHARDS_OUTPUT_COST_MEMBERS':str(n),'SHARDS_OUTPUT_COST_DEADLINE_MS':'300000','SHARDS_OUTPUT_COST_RECORDS':str(d/'records')}\n (d/'command.json').write_text(json.dumps(cmd)+'\\n');(d/'environment.json').write_text(json.dumps(extra,indent=2)+'\\n')\n with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=env|extra,stdout=out,stderr=subprocess.STDOUT)\n (d/'test.exit').write_text(str(p.returncode)+'\\n');results.append({'case':name,'members':n,'exit':p.returncode});(r/'results.json').write_text(json.dumps(results,indent=2)+'\\n');print(name,p.returncode,flush=True)\n if p.returncode:raise SystemExit(p.returncode)\n(r/'driver.exit').write_text('0\\n')\n")
os.chdir(root)
subprocess.run(['python3',str(p)],check=True)
