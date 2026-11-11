from pathlib import Path
import json,os,subprocess,time
root=Path.home()/'managed-audit-wire-reserved-close-v1'
r=root/'case-controller-v1'
dispatch=json.loads((r/'dispatch-before-case.json').read_text())
budget=dispatch['budget']; ready=dispatch['ready']
assert time.time()+45 < budget['deadline_epoch']
s=os.statvfs(root);assert s.f_bavail*s.f_frsize>=30*1024**3
with (r/'actor-start.json').open('x') as f:json.dump({'epoch':time.time(),'native_case_attempt':1,'argv':ready['argv'],'deadline_epoch':budget['deadline_epoch']},f,indent=2)
env=os.environ.copy()
for k in list(env):
 if k.startswith('DYLD_'):del env[k]
env.update(ready['case_environment'])
start=time.time()
with (r/'case.stdout').open('xb') as out,(r/'case.stderr').open('xb') as err:
 try:
  c=subprocess.run(ready['argv'],cwd=root/'source',env=env,stdout=out,stderr=err,timeout=45)
  code=c.returncode;expired=False
 except subprocess.TimeoutExpired:
  code=None;expired=True
result={'exit_code':code,'timeout':expired,'elapsed_s':time.time()-start,'native_case_attempts':1,'native_retries':0,'original_deadline_epoch':budget['deadline_epoch'],'cleanup':'Unknown_if_parent_timeout_otherwise_leaf_receipts_authoritative'}
with (r/'case.result.json').open('x') as f:json.dump(result,f,indent=2)
print(json.dumps(result),flush=True)
print('STDOUT\n'+(r/'case.stdout').read_text(),flush=True)
print('STDERR\n'+(r/'case.stderr').read_text(),flush=True)
events=root/'native-records-v1/events.log'
if events.exists():print('EVENTS\n'+events.read_text(),flush=True)
raise SystemExit(0 if code==0 and not expired else 1)
