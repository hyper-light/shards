from pathlib import Path
import base64,zlib,json,hashlib
home=Path.home();before=home/'root-managed-terminal-before-26ba-v1';after=home/'root-managed-terminal-after-b02e-v2'
files={};manifest=[]
def collect(root,phase,names):
 for name in names:
  p=root/name
  if not p.is_file():continue
  data=p.read_bytes();key=phase+'/'+name;files[key]=data.decode()
  manifest.append({'path':key,'original':str(p),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
shared=['source.txt','compile.exit','driver.exit','results.json','test-artifact.json']
collect(after,'after',shared)
for case in ['normal','stdio']:
 names=['command.json','test.exit','test.log','records/inputs.json','records/managed-output.before-stop','records/managed-output.stopped','records/teardown.json','records/stop.json','records/stopped-state.json','records/remove.json','records/run-cleanup.json']
 if case=='normal':names += ['records/member-ended.json','records/managed-output.member-ended','records/neighbor-progress.json']
 else:names += ['records/actor-eof.json','records/managed-output.actor-eof','records/actor-control.json','records/actor-live-after-eof.json','records/subsequent-progress.json']
 collect(before,'before',[case+'/'+n for n in names]);collect(after,'after',[case+'/'+n for n in names])
for kind in ['ordinary','daemon-unit','remote-unit']:
 collect(after,'after',[kind+'.command.json',kind+'.exit',kind+'.log'])
files['MANIFEST.json']=json.dumps(manifest,indent=2)+'\n'
encoded=base64.b64encode(zlib.compress(json.dumps(files).encode(),9)).decode()
print(json.dumps({'base64':encoded,'base64_bytes':len(encoded),'members':len(manifest)}))
