from pathlib import Path
import hashlib,json,os,time
root=Path.home()/'managed-audit-wire-native-v1'
b=json.loads((root/'budget.json').read_text())
ready=json.loads((root/'build-records/ready.json').read_text())
assert time.time()+45 < b['deadline_epoch']
assert ready['selected_count']==1 and ready['native_cases_run']==0
assert ready['sha256']=='a502bd28e1e55f27ff204788c3737fe148c5ceb5441e6f9287f56e15ba8cea30'
assert hashlib.sha256(Path(ready['executable']).read_bytes()).hexdigest()==ready['sha256']
listed=(root/'build-records/list.stdout').read_text()
assert [line for line in listed.splitlines() if line.endswith(': test')]==[ready['filter']+': test']
stages={}
for name in ['toolchain','policy','build','list','clippy']:
 data=json.loads((root/('build-records/'+name+'.result.json')).read_text())
 assert data['exit_code']==0 and not data['timeout']
 stages[name]=data
usage=os.statvfs(root)
assert usage.f_bavail*usage.f_frsize >=30*1024**3
assert not (root/'native-records-v1').exists()
(root/'native-tmp-v1').mkdir()
controller=root/'case-controller-v1'
controller.mkdir()
receipt={'status':'DispatchRecorded_BeforeSingleCase','at_epoch':time.time(),'budget':b,'ready':ready,'stages':stages,'free_bytes':usage.f_bavail*usage.f_frsize,
'package_sha256':hashlib.sha256((root/'source-package.tar.gz').read_bytes()).hexdigest(),
'leaf_sha256':hashlib.sha256((root/'source/docs/research/measurements/managed-audit-wire-adversarial/native.rs').read_bytes()).hexdigest(),
'lib_include_sha256':hashlib.sha256((root/'source/crates/vmm/src/lib.rs').read_bytes()).hexdigest(),
'case_native_count':0,'runtime_retry_allowed':False,'parent_hard_seconds':45,'work_seconds':30,'cleanup_allowance_seconds':15}
with (controller/'dispatch-before-case.json').open('x') as f: json.dump(receipt,f,indent=2)
print(json.dumps(receipt),flush=True)
