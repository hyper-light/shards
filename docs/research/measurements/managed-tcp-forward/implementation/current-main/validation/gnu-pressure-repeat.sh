python3 - <<'PY'
from pathlib import Path
import os,resource,subprocess,time,signal,select,json,hashlib
home=Path.home();old=home/'relay-current-1be-a920-light-v2';root=home/'relay-current-1be-gnu-pressure-repeat-v1';root.mkdir(exist_ok=False)
os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:2]));resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));resource.setrlimit(resource.RLIMIT_CORE,(0,0))
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
source=old/'source';binary=old/'artifacts/x86_64-unknown-linux-gnu';driver=old/'record/original-native_cases.py';given=old/'record/x86_64-unknown-linux-gnu-pressure-backend-fin.inputs.json';manifest=json.loads((source/'docs/research/measurements/managed-tcp-forward/implementation/current-main/source-identities.json').read_text())
assert digest(binary)=='0ba3e64e24bc900fc02a036814c0c8990df9cafe29b8e0ca86ceee760b14dc09'
assert digest(driver)=='eef969298d83c23dc6247131c9ae14078971ab732b47f0c25864f55218838fd9'
identities={r['path']:digest(source/r['path']) for r in manifest['paths']};assert all(identities[r['path']]==r['sha256'] for r in manifest['paths'])
cmd=['python3',str(driver),'--case','both-half-closes','--binary',str(binary),'--inputs',str(given),'--record',str(root/'case')]
(root/'command.json').write_text(json.dumps(cmd,indent=2));before=len(os.listdir('/proc/self/fd'));child=None;pidfd=None;forced=False;ready=False;status=None
try:
 with (root/'driver.log').open('wb') as output:
  child=subprocess.Popen(cmd,cwd=source,stdout=output,stderr=subprocess.STDOUT,start_new_session=True);pidfd=os.pidfd_open(child.pid,0)
  try:status=child.wait(timeout=60)
  except subprocess.TimeoutExpired:forced=True;os.killpg(child.pid,signal.SIGKILL);status=child.wait()
  poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
finally:
 if child is not None and child.returncode is None:forced=True;os.killpg(child.pid,signal.SIGKILL);child.wait()
 if pidfd is not None:os.close(pidfd)
case=json.loads((root/'case/result.json').read_text());obs=json.loads((root/'case/observed.json').read_text());compact=[]
for row in obs:
 if row.get('type')=='Endpoint':
  data=bytes.fromhex(row['read_hex']);row={k:v for k,v in row.items() if k!='read_hex'}|{'read_bytes':len(data),'read_sha256':hashlib.sha256(data).hexdigest()}
 compact.append(row)
afterids={r['path']:digest(source/r['path']) for r in manifest['paths']};tracked=subprocess.run(['git','diff','--exit-code'],cwd=source,capture_output=True)
result={'source_revision':'1be384ecb3e3852a6b97daaf1ead61d441f21d20','unchanged_input_sha256':digest(given),'unchanged_driver_sha256':digest(driver),'binary_before_sha256':'0ba3e64e24bc900fc02a036814c0c8990df9cafe29b8e0ca86ceee760b14dc09','binary_after_sha256':digest(binary),'selected_source_before':identities,'selected_source_after':afterids,'tracked_diff_status':tracked.returncode,'fd_before':before,'fd_after':len(os.listdir('/proc/self/fd')),'driver_status':status,'driver_pidfd_exit_ready':ready,'driver_reaped':child.returncode is not None,'forced_owned_cleanup':forced,'case':case,'observed':compact,'no_actual_vm':True,'kernel_return':'Unknown','physical_kernel_caps':'Unverified','passed':status==0 and case['passed'] and ready and not forced and identities==afterids and tracked.returncode==0}
(root/'result.json').write_text(json.dumps(result,indent=2));print(json.dumps(result));raise SystemExit(0 if result['passed'] else 1)
PY
