from pathlib import Path
import os,subprocess,json,hashlib,time,difflib,shutil,traceback
old=Path.home()/'managed-intercept-5b7d-tcp6-mutations-v2'
(old/'driver.exit').write_text('1\n')
(old/'root-review.txt').write_text('r336 expected all OUTPUT-mark removals to bypass. Actual local cargo101 backend2/2 relay0; loopback cargo0 backend1/relay1. Campaign assumption failed, not a product finding. Source restored byte-identically. No fallback mutation or closing green executed in v2. v3 tests actual PRE rule bundle for same-namespace targets and retains independent OUTPUT-self observation.\n')
root=Path.home()/'managed-intercept-5b7d-tcp6-mutations-v3';root.mkdir(exist_ok=False)
shutil.copyfile(__file__,root/'driver.py')
(root/'docker-empty').mkdir();os.environ['DOCKER_CONFIG']=str(root/'docker-empty')
source=Path('crates/testguest/src/managed_network/intercept/wire.rs');original=source.read_bytes();(root/'wire.saved').write_bytes(original)
(root/'identity').write_text(subprocess.check_output(['git','rev-parse','HEAD'],text=True)+subprocess.check_output(['uname','-a'],text=True))
(root/'original.sha256').write_text(hashlib.sha256(original).hexdigest()+'\n')
results=[];errors=[]
def changed(name,old,new):
 assert original.count(old)==1,(name,original.count(old))
 content=original.replace(old,new);source.write_bytes(content);source.touch()
 (root/(name+'.patch')).write_text(''.join(difflib.unified_diff(original.decode().splitlines(True),content.decode().splitlines(True),fromfile=str(source),tofile=str(source))))
def run(name,origin):
 env=dict(os.environ,SHARDS_INTERCEPT_ORIGIN=origin,SHARDS_NETWORK_RECORDS=str(root/name));t=time.monotonic_ns()
 command=['bash','docs/research/measurements/managed-network-intercept/run.sh'];(root/(name+'.command.json')).write_text(json.dumps(command)+'\n')
 with (root/(name+'.driver.log')).open('w') as out: rc=subprocess.run(command,env=env,stdout=out,stderr=subprocess.STDOUT).returncode
 p=root/name/('intercept-tcp6-'+origin+'.stdout');events=[json.loads(l[len('managed-intercept '):]) for l in p.read_text().splitlines() if l.startswith('managed-intercept ')] if p.exists() else []
 (root/(name+'.observed.json')).write_text(json.dumps(events,indent=2)+'\n');row=dict(case=name,origin=origin,exit=rc,elapsed_ns=time.monotonic_ns()-t);results.append(row);(root/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(row),flush=True)
 return rc,events
try:
 changed('output-removed',b'let output = if input.origin != Origin::Forward',b'let output = if false')
 rc,events=run('output-removed-local-self','local-self');joined=[e for e in events if e.get('type')=='Joined'];assert len(joined)==1 and joined[0]['joined'] is True
 if rc==0:assert joined[0]['backend_accepted']==1 and joined[0]['relay_accepted']==1
 else:assert rc==101 and joined[0]['backend_accepted']==2 and joined[0]['backend_receipts']==2 and joined[0]['relay_accepted']==0
 (root/'output-self-necessity.json').write_text(json.dumps(dict(cargo_exit=rc,joined=joined[0],scope='observed necessity of OUTPUT mark for this exact self-address path; not a mandatory guard-removal pass'),indent=2)+'\n')
 changed('prerouting-selected-bundle-removed',b'        intercept,\n        rule(family, b"pre\\0", expressions)?,\n',b'')
 for origin in ['local-loopback','local-self']:
  rc,events=run('prerouting-removed-'+origin,origin);joined=[e for e in events if e.get('type')=='Joined'];assert rc==101 and len(joined)==1,(origin,rc,joined)
  assert joined[0]['backend_accepted']==2 and joined[0]['backend_receipts']==2 and joined[0]['relay_accepted']==0 and joined[0]['joined'] is True,(origin,joined)
 changed('fallback-removed',b'        rule(family, b"pre\\0", expressions)?,\n',b'')
 rc,events=run('fallback-removed','forward-relay-absent');joined=[e for e in events if e.get('type')=='Joined'];assert rc==101 and len(joined)==1
 assert joined[0]['backend_accepted']==2 and joined[0]['backend_receipts']==2 and joined[0]['relay_accepted']==0 and joined[0]['joined'] is True
except BaseException:
 errors.append(traceback.format_exc());(root/'mutation-error.txt').write_text(errors[-1]);print(errors[-1],flush=True)
finally:
 source.write_bytes(original);source.touch();assert source.read_bytes()==original
 (root/'restored.sha256').write_text(hashlib.sha256(source.read_bytes()).hexdigest()+'\n');(root/'restored.diff').write_bytes(subprocess.check_output(['git','diff','--',str(source)]))
for origin in ['local','local-loopback','local-self','forward','forward-relay-absent']:
 try:
  rc,events=run('restored-'+origin,origin);assert rc==0,(origin,rc)
 except BaseException:
  errors.append(traceback.format_exc());(root/('restored-'+origin+'.error.txt')).write_text(errors[-1]);print(errors[-1],flush=True)
(root/'driver.exit').write_text('1\n' if errors else '0\n');raise SystemExit(1 if errors else 0)
