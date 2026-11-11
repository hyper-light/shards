#!/usr/bin/env python3
"""Run each authorized native case once, after a durable preactor receipt."""
from pathlib import Path
import gzip,hashlib,io,json,os,resource,select,signal,subprocess,tarfile,time,traceback
ROOT=Path('/home/adalundhe/relay-normal-late-request-0a6-v1')
REC=ROOT/'record'
START=json.loads((REC/'STARTED.json').read_text()); END=START['original_end_monotonic_ns']
def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def save(path,obj):
    with path.open('x') as f: json.dump(obj,f,indent=2); f.write('\n')
def remaining(): return (END-time.monotonic_ns())/1e9
before=len(os.listdir('/proc/self/fd')); started=0
result={'native_cases_started':0,'case_order':['late-request','forward-server-first'],'runtime_retry':False,'mutation':False,'physical_kernel_caps':'Unverified','kernel_population_return':'Unknown','controller_fd_before':before,'cases':[]}
try:
    if Path('/proc/sys/kernel/random/boot_id').read_text().strip()!=START['boot_id']: raise RuntimeError('different boot')
    os.sched_setaffinity(0,sorted(os.sched_getaffinity(0))[:2]); resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));resource.setrlimit(resource.RLIMIT_CORE,(0,0))
    ready=json.loads((REC/'READY.json').read_text()); spec=json.loads((ROOT/'packet/SOURCE.json').read_text())
    binary=Path(ready['binary'])
    if ready['status']!='READY_NATIVE_UNRUN' or sha(binary)!=ready['binary_sha256']: raise RuntimeError('ready/binary identity')
    if sha(ROOT/'source-package-v1.tar.gz')!=START['source_package_sha256']: raise RuntimeError('package changed')
    if sha(ROOT/'packet/native_case.py')!=ready['driver_sha256']: raise RuntimeError('driver changed')
    closure=[{'path':x['path'],'sha256':sha(ROOT/'source'/x['path']),'bytes':(ROOT/'source'/x['path']).stat().st_size} for x in spec['source_closure']]
    if closure!=json.loads((REC/'source-before.json').read_text()): raise RuntimeError('source changed before actors')
    if not (REC/'ROOT-PREACTOR-RECEIPT.json').is_file(): raise RuntimeError('ROOT source/binary/list1 receipt missing')
    save(REC/'actor-start-identities.json',{'source_closure':closure,'binary_sha256':sha(binary),'driver_sha256':sha(ROOT/'packet/native_case.py'),'package_sha256':sha(ROOT/'source-package-v1.tar.gz'),'root_receipt':json.loads((REC/'ROOT-PREACTOR-RECEIPT.json').read_text()),'original_remaining_ns':END-time.monotonic_ns(),'affinity':sorted(os.sched_getaffinity(0)),'rlimit_as':resource.getrlimit(resource.RLIMIT_AS)})
    for case in result['case_order']:
        if remaining()<45: raise TimeoutError('original total budget cannot admit next original45s parent: '+case)
        if (ROOT/'cases'/case).exists(): raise RuntimeError('existing case path: refusing runtime retry')
        command=['python3',str(ROOT/'packet/native_case.py'),'--case',case,'--binary',str(binary),'--inputs',str(ROOT/'packet/inputs.json'),'--record',str(ROOT/'cases'/case)]
        held=None;child=None;forced=False;exitready=False; casebefore=len(os.listdir('/proc/self/fd'))
        start=time.monotonic_ns(); entry={'case':case,'command':command,'binary_before_sha256':sha(binary),'original_remaining_ns_before':END-start}
        with (REC/('driver-'+case+'.stdout')).open('xb') as out,(REC/('driver-'+case+'.stderr')).open('xb') as err:
            try:
                child=subprocess.Popen(command,stdout=out,stderr=err,start_new_session=True)
                started+=1; result['native_cases_started']=started
                held=os.pidfd_open(child.pid,0); poll=select.poll();poll.register(held,select.POLLIN)
                events=poll.poll(max(0,int(remaining()*1000)))
                exitready=any(fd==held and bits&select.POLLIN for fd,bits in events)
                if not exitready: raise TimeoutError('original total deadline while driver '+case)
                status=child.wait(timeout=max(0,remaining()))
            finally:
                if child is not None and child.returncode is None:
                    forced=True;os.killpg(child.pid,signal.SIGKILL);child.wait()
                if held is not None:os.close(held)
        entry.update(driver_status=child.returncode,pid=child.pid,pidfd_exit_ready=exitready,direct_reaped=child.returncode is not None,forced_owned_cleanup=forced,fd_before=casebefore,fd_after=len(os.listdir('/proc/self/fd')),binary_after_sha256=sha(binary),elapsed_ns=time.monotonic_ns()-start,original_remaining_ns_after=END-time.monotonic_ns())
        native=ROOT/'cases'/case/'result.json'
        entry['native_result']=json.loads(native.read_text()) if native.is_file() else None
        save(REC/('case-'+case+'.json'),entry);result['cases'].append(entry)
        print(json.dumps({'case':case,'driver_status':child.returncode,'native_result':entry['native_result']}),flush=True)
        if forced or child.returncode!=0 or entry['native_result'] is None or not entry['native_result']['passed']: raise RuntimeError('first actual case failure ends batch; no retry or later control')
    after=[{'path':x['path'],'sha256':sha(ROOT/'source'/x['path']),'bytes':(ROOT/'source'/x['path']).stat().st_size} for x in spec['source_closure']]
    save(REC/'source-after-actors.json',after)
    if after!=closure: raise RuntimeError('source altered during actors')
    result.update(status='COMPLETE',source_unchanged=True,binary_sha256=sha(binary),passed=all(c['driver_status']==0 and c['native_result'] is not None and c['native_result']['passed'] for c in result['cases']))
except Exception as error:
    result.update(status='INCOMPLETE',passed=False,error={'type':type(error).__name__,'detail':str(error)},traceback=traceback.format_exc())
result.update(controller_fd_after=len(os.listdir('/proc/self/fd')),original_remaining_ns=END-time.monotonic_ns())
save(REC/'FINAL.json',result)
# Archive retained records only. Frozen source, binaries and private build outputs remain untouched.
files=[]
for area in ('record','packet','cases'):
    parent=ROOT/area
    if parent.exists():
        for path in sorted(parent.rglob('*')):
            if path.is_symlink(): raise RuntimeError('unexpected symlink in raw records')
            if path.is_file(): files.append(path)
manifest={'files':[{'path':str(p.relative_to(ROOT)),'bytes':p.stat().st_size,'sha256':sha(p)} for p in files],'excluded_retained':['source/','target/','cargo-home/','check/','artifacts/'],'source_package_sha256':START['source_package_sha256'],'native_cases_started':started}
manifest_bytes=(json.dumps(manifest,indent=2)+'\n').encode()
archive=ROOT/'raw-linux.tar.gz'
with archive.open('xb') as raw:
    with gzip.GzipFile(filename='',mode='wb',fileobj=raw,mtime=0) as gz:
        with tarfile.open(mode='w',fileobj=gz) as tf:
            for path in files:
                b=path.read_bytes();info=tarfile.TarInfo(str(path.relative_to(ROOT)));info.size=len(b);info.mode=0o600;tf.addfile(info,io.BytesIO(b))
            info=tarfile.TarInfo('manifest.json');info.size=len(manifest_bytes);info.mode=0o600;tf.addfile(info,io.BytesIO(manifest_bytes))
metadata={'path':str(archive),'bytes':archive.stat().st_size,'sha256':sha(archive),'payload_count':len(files),'total_members':len(files)+1,'result':result}
save(ROOT/'archive-metadata.json',metadata)
print(json.dumps(metadata),flush=True)
raise SystemExit(0 if result['passed'] and result['controller_fd_before']==result['controller_fd_after'] else 1)
