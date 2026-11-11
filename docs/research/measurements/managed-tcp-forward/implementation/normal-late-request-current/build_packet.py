#!/usr/bin/env python3
"""Preactor projection build; retain the packet's original absolute budget."""
from pathlib import Path
import hashlib,json,os,resource,select,shutil,signal,subprocess,time,tomllib,traceback
ROOT=Path('/home/adalundhe/relay-normal-late-request-0a6-v1')
REC=ROOT/'record'
START=json.loads((REC/'STARTED.json').read_text())
END=START['original_end_monotonic_ns']
BASE='0a6d91b3de0f10a8397868aa3794adab1e09afdc'
SRC=ROOT/'source'
ENV=os.environ.copy()
ENV.update(CARGO_HOME=str(ROOT/'cargo-home'), CARGO_TARGET_DIR=str(ROOT/'target'), CARGO_BUILD_JOBS='2', NUM_JOBS='2', CARGO_INCREMENTAL='0', RUSTUP_TOOLCHAIN='1.98.1', CLIPPY_CONF_DIR=str(SRC))
for key in list(ENV):
    if key.startswith('DYLD_'): del ENV[key]
def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def save(name,obj):
    with (REC/name).open('x') as f: json.dump(obj,f,indent=2); f.write('\n')
def remaining(): return (END-time.monotonic_ns())/1e9
def phase(name,command,cwd=ROOT,allowed=(0,)):
    if remaining()<=0: raise TimeoutError('original packet budget expired before '+name)
    before=len(os.listdir('/proc/self/fd')); began=time.monotonic_ns()
    child=None; pidfd=None; forced=False; ready=False
    with (REC/(name+'.stdout')).open('xb') as out,(REC/(name+'.stderr')).open('xb') as err:
        try:
            child=subprocess.Popen(command,cwd=cwd,env=ENV,stdout=out,stderr=err,start_new_session=True)
            pidfd=os.pidfd_open(child.pid,0); poll=select.poll(); poll.register(pidfd,select.POLLIN)
            events=poll.poll(max(0,int(remaining()*1000)))
            ready=any(fd==pidfd and bits&select.POLLIN for fd,bits in events)
            if not ready: raise TimeoutError('original packet deadline during '+name)
            status=child.wait(timeout=max(0,remaining()))
        finally:
            if child is not None and child.returncode is None:
                forced=True; os.killpg(child.pid,signal.SIGKILL); child.wait()
            if pidfd is not None: os.close(pidfd)
    result={'name':name,'command':command,'cwd':str(cwd),'status':child.returncode,'pid':child.pid,'pidfd_exit_ready':ready,'direct_reaped':child.returncode is not None,'forced_owned_cleanup':forced,'fd_before':before,'fd_after':len(os.listdir('/proc/self/fd')),'elapsed_ns':time.monotonic_ns()-began,'original_remaining_ns':END-time.monotonic_ns()}
    save(name+'.json',result); print(json.dumps(result),flush=True)
    if result['status'] not in allowed or not ready or forced or result['fd_before']!=result['fd_after']: raise RuntimeError('phase failed: '+name)
    return (REC/(name+'.stdout')).read_text()
result={'native_cases_started':0,'physical_kernel_caps':'Unverified','kernel_population_return':'Unknown'}
try:
    if Path('/proc/sys/kernel/random/boot_id').read_text().strip()!=START['boot_id']: raise RuntimeError('different boot')
    if remaining()<=0: raise TimeoutError('original packet deadline expired')
    os.sched_setaffinity(0,sorted(os.sched_getaffinity(0))[:2]); resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3)); resource.setrlimit(resource.RLIMIT_CORE,(0,0))
    if shutil.disk_usage(ROOT).free<4*1024**3: raise RuntimeError('private admission disk unavailable')
    result.update(affinity=sorted(os.sched_getaffinity(0)),rlimit_as=resource.getrlimit(resource.RLIMIT_AS),rlimit_core=resource.getrlimit(resource.RLIMIT_CORE),fd_before=len(os.listdir('/proc/self/fd')))
    save('preactor-environment.json',{'uname':list(os.uname()),'env':{k:ENV[k] for k in ['CARGO_HOME','CARGO_TARGET_DIR','CARGO_BUILD_JOBS','NUM_JOBS','CARGO_INCREMENTAL','RUSTUP_TOOLCHAIN','CLIPPY_CONF_DIR']},'limits':result})
    spec=json.loads((ROOT/'packet/SOURCE.json').read_text())
    for file,expected in [('late-request.patch',spec['source_patch_sha256']),('guarded.patch',spec['guarded_patch_sha256']),('native_case.py',spec['driver_sha256']),('inputs.json',spec['inputs_sha256'])]:
        if sha(ROOT/'packet'/file)!=expected: raise RuntimeError('packet identity '+file)
    SRC.mkdir(exist_ok=False)
    phase('git-init',['git','init'],SRC)
    phase('git-remote',['git','remote','add','origin','git@github.com:hyper-light/shards.git'],SRC)
    phase('git-sparse-init',['git','sparse-checkout','init','--cone'],SRC)
    phase('git-sparse-set',['git','sparse-checkout','set','crates/testguest/src/managed_network','crates/init/src','crates/devcgroup','crates/shards/tests/managed_network','docs/research/measurements/managed-tcp-forward/implementation/engine-native','scripts','resources','.github/workflows','.cargo','docs/research/measurements/managed-network-intercept'],SRC)
    phase('git-fetch',['git','fetch','--depth=1','--filter=blob:none','origin',BASE],SRC)
    phase('git-checkout',['git','checkout','--detach',BASE],SRC)
    phase('git-apply-guarded',['git','apply',str(ROOT/'packet/guarded.patch')],SRC)
    if sha(SRC/spec['rust_source_change'])!=spec['guarded_source_sha256']: raise RuntimeError('guarded d5b base mismatch')
    phase('git-apply-new-function',['git','apply',str(ROOT/'packet/late-request.patch')],SRC)
    identities=[]
    for entry in spec['source_closure']:
        path=SRC/entry['path']; actual=sha(path)
        if actual!=entry['sha256'] or path.stat().st_size!=entry['bytes']: raise RuntimeError('source identity '+entry['path'])
        identities.append({'path':entry['path'],'sha256':actual,'bytes':path.stat().st_size})
    save('source-before.json',identities)
    diff=phase('source-diff',['git','diff','--binary','--no-ext-diff','--abbrev=9'],SRC)
    changed=phase('source-changed-paths',['git','diff','--name-only'],SRC).splitlines()
    if changed!=[spec['rust_source_change']]: raise RuntimeError('changes outside owned source path')
    phase('policy',['python3','scripts/check-development-policy'],SRC)
    phase('format',['rustfmt','--check','--edition','2024','--config','skip_children=true','crates/testguest/src/managed_network/intercept/tcp_forward.rs'],SRC)
    phase('toolchain',['rustc','-vV'],SRC)
    rootlib=SRC/'crates/testguest/src/interface_root.rs'
    with rootlib.open('x') as f: f.write('#![allow(dead_code)]\nmod managed_network;\n')
    check=ROOT/'check';check.mkdir(exist_ok=False)
    cargo='[package]\nname="relay-reverse-extra-check"\nversion="0.0.0"\nedition="2024"\nrust-version="1.98.1"\n[workspace]\n[lib]\npath="../source/crates/testguest/src/interface_root.rs"\n[dependencies]\nlibc="=0.2.189"\nshards-devcgroup={path="../source/crates/devcgroup"}\n'
    (check/'Cargo.toml').write_text(cargo)
    save('projection.json',{'rootlib':{'path':str(rootlib),'sha256':sha(rootlib),'text':rootlib.read_text()},'manifest':{'path':str(check/'Cargo.toml'),'sha256':sha(check/'Cargo.toml'),'text':cargo}})
    common=['--manifest-path',str(check/'Cargo.toml'),'--target','x86_64-unknown-linux-gnu','-j2']
    phase('check',['cargo','check','--lib',*common],SRC)
    cfg=tomllib.loads((SRC/'Cargo.toml').read_text())['workspace']['lints']
    flags=[]
    for group in ('rust','clippy'):
        for key,detail in cfg[group].items():
            level=detail if isinstance(detail,str) else detail['level']
            flags.extend([{'allow':'-A','warn':'-W','deny':'-D','forbid':'-F'}[level],('clippy::' if group=='clippy' else '')+key])
    phase('clippy',['cargo','clippy','--lib','--all-targets',*common,'--',*flags,'-Dwarnings'],SRC)
    raw=phase('test-build',['cargo','test','--lib','--no-run',*common,'--message-format=json'],SRC)
    artifacts=[x for line in raw.splitlines() if line.startswith('{') for x in [json.loads(line)] if x.get('reason')=='compiler-artifact' and x.get('profile',{}).get('test') and x.get('executable')]
    if len(artifacts)!=1: raise RuntimeError('expected exactly one test executable')
    original=Path(artifacts[0]['executable']); frozen=ROOT/'artifacts'; frozen.mkdir(exist_ok=False)
    binary=frozen/'native-gnu';
    with binary.open('xb') as f: f.write(original.read_bytes())
    binary.chmod(0o500)
    if sha(binary)!=sha(original): raise RuntimeError('frozen binary copy differs')
    listings={}
    for name,exact in spec['case_filters'].items():
        listing=phase('list-'+name,[str(binary),'--list','--ignored','--exact',exact],SRC)
        if exact+': test' not in listing or '1 test, 0 benchmarks' not in listing: raise RuntimeError('exact list1 absent '+name)
        listings[name]=listing
    after=[{'path':x['path'],'sha256':sha(SRC/x['path']),'bytes':(SRC/x['path']).stat().st_size} for x in spec['source_closure']]
    save('source-after-build.json',after)
    if after!=identities: raise RuntimeError('source altered during build')
    result.update(status='READY_NATIVE_UNRUN',base=BASE,source_package_sha256=sha(ROOT/'source-package-v1.tar.gz'),fixture_sha256=sha(SRC/spec['rust_source_change']),driver_sha256=sha(ROOT/'packet/native_case.py'),binary=str(binary),binary_sha256=sha(binary),binary_bytes=binary.stat().st_size,listings=listings,source_unchanged=True)
    result['fd_after']=len(os.listdir('/proc/self/fd'))
    if result['fd_before']!=result['fd_after']: raise RuntimeError('preactor FD mismatch')
    result['original_remaining_ns']=END-time.monotonic_ns(); save('READY.json',result)
except Exception as error:
    result.update(status='PREACTOR_FAILED',error={'type':type(error).__name__,'detail':str(error)},traceback=traceback.format_exc(),original_remaining_ns=END-time.monotonic_ns(),fd_after=len(os.listdir('/proc/self/fd')))
    save('PREACTOR-FAILED.json',result)
print(json.dumps(result),flush=True)
raise SystemExit(0 if result['status']=='READY_NATIVE_UNRUN' else 1)
