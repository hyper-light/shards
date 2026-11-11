from pathlib import Path
import hashlib,json,os,time
root=Path.home()/'managed-audit-wire-reserved-close-v1'
b=json.loads((root/'budget.json').read_text())
ready=json.loads((root/'build-records/ready.json').read_text())
assert time.time()+45 < b['deadline_epoch']
assert ready['selected_count']==1 and ready['native_cases_run']==0
assert ready['sha256']=='85a6853df89ab49b01b6823ff9216492441e2445fb067773adf3cf1258dfc6e5'
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
assert hashlib.sha256((root/'source/docs/research/measurements/managed-audit-wire-adversarial/native.rs').read_bytes()).hexdigest()=='228bb9e67e1e699d969a71de817c5a9c6e461ee18e07d98aa070029b07b59e4e'
assert hashlib.sha256((root/'source-package.tar.gz').read_bytes()).hexdigest()=='37448e07bd227d19346d990d188bfe46ed1f0a43c1b789666262c82d7003970f'
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
