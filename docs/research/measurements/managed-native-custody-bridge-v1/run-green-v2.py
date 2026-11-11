#!/usr/bin/env python3
"""ROOT custody presentation v2: GREEN-v2 check/strict/fmt/policy, then the TWO cases on GREEN-v2,
on RED-v10d (v10d routing), RED-v1 (first ROOT routing) and a custody-dropping MUTANT-v2.
ZERO helper/actor/native child/VM/cgroup effects; stops at the first unexpected result."""
from pathlib import Path
import hashlib,json,os,re,resource,select,shutil,signal,subprocess,sys,tarfile,time

began=time.monotonic(); deadline=float(sys.argv[3]); budget=min(600,deadline-began); cleanup=15
assert budget>cleanup
package=Path(sys.argv[1]).resolve(); scope=Path(sys.argv[2]).expanduser().resolve()
assert os.uname().sysname=='Linux' and os.uname().machine=='x86_64' and os.geteuid()!=0
assert shutil.disk_usage(scope.parent).free>=4*1024**3
scope.mkdir(exist_ok=False); records=scope/'records';records.mkdir()
import zlib,base64
spec=json.loads(zlib.decompress(package.read_bytes()))
template=Path.home()/'Projects/managed-native-owner-runtime-01a1c99e-v9b'
assert json.loads((template/'records/final.json').read_text())['exit']==101
assert hashlib.sha256((template/'source/FROZEN-SOURCE.json').read_bytes()).hexdigest()==spec['base_source_manifest_sha256']

def tree_manifest(path):
    files={}
    for p in sorted(path.rglob('*')):
        rel=p.relative_to(path).as_posix()
        if p.is_symlink():
            data=os.readlink(p).encode();kind='symlink'
        elif p.is_file():
            data=p.read_bytes();kind='file'
        else:continue
        files[rel]={'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'kind':kind}
    return files

template_before=tree_manifest(template/'source')
cache_before={name:tree_manifest(template/'cargo-home'/name) for name in ('registry','git')}
(records/'template-cache-before.json').write_text(json.dumps({'template':template_before,'cache':cache_before},sort_keys=True)+'\n')

source=scope/'source'
shutil.copytree(template/'source',source,symlinks=True)
for name,pin in spec['files'].items():
    rel=Path(name);assert not rel.is_absolute() and '..' not in rel.parts
    p=source/rel;assert not p.is_symlink()
    if pin['before_sha256'] is not None:assert hashlib.sha256(p.read_bytes()).hexdigest()==pin['before_sha256']
    d=base64.b64decode(pin['base64'],validate=True);assert len(d)==pin['bytes'] and hashlib.sha256(d).hexdigest()==pin['sha256']
    p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(d)
manifest=json.loads((template/'source/FROZEN-SOURCE.json').read_text())
manifest['meaning']=spec['new_source_meaning']
manifest['policy']=spec['new_policy']
for name,pin in spec['files'].items():
    manifest['files'][name]={'bytes':pin['bytes'],'sha256':pin['sha256'],'kind':pin['kind']}
manifest['files']=dict(sorted(manifest['files'].items(),key=lambda item:Path(item[0])))
encoded=json.dumps(manifest,indent=2)+'\n'
assert hashlib.sha256(encoded.encode()).hexdigest()==spec['new_source_manifest_sha256']
(source/'FROZEN-SOURCE.json').write_text(encoded)
for name,pin in manifest['files'].items():
    p=source/name;d=os.readlink(p).encode() if p.is_symlink() else p.read_bytes()
    assert len(d)==pin['bytes'] and hashlib.sha256(d).hexdigest()==pin['sha256'],name
(records/'source-verified.json').write_text(json.dumps({'files':len(manifest['files']),'capsule_sha256':hashlib.sha256(package.read_bytes()).hexdigest(),'local_full_source_archive_sha256':spec['local_full_archive_sha256'],'base':'own frozen v9bb, read only; all final source hashes reverified','scope':'actual parent source plus frozen real producer/consumer changes, not stubs'},indent=2)+'\n')
cargo=scope/'cargo-home';cargo.mkdir()
for name in ('registry','git'):shutil.copytree(template/'cargo-home'/name,cargo/name,symlinks=False)
temporary=scope/'tmp';temporary.mkdir()
allowed=sorted(os.sched_getaffinity(0));assert len(allowed)>=2
os.sched_setaffinity(0,set(allowed[:2]))
env={k:v for k,v in os.environ.items() if not k.startswith('DYLD_')}
env.update(CARGO_HOME=str(cargo),CARGO_TARGET_DIR=str(scope/'target'),CARGO_BUILD_JOBS='2',CARGO_INCREMENTAL='0',TMPDIR=str(temporary))
identity={'source':manifest['meaning'],'policy':manifest['policy'],'uname':list(os.uname()),'uid':os.geteuid(),'cpus':sorted(os.sched_getaffinity(0)),
'envelope':'normal 2CPU/2GiB/4GiB/600s including cleanup/-j2; no physical backing or descendant return proof',
'cache':'private byte copies of own frozen v9bb registry/git caches ONLY; no template source/target mutation or result inheritance',
'forbidden':'no helper/actor/native execution, host cgroup writes, VM, held/skipped campaigns, profiling or benchmarks; the only mutation is the named run.rs MUTANT inside this private source'}
(records/'identity.json').write_text(json.dumps(identity,indent=2)+'\n');shutil.copy2(__file__,records/'driver.py')

def child_limit():resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3))
def run(name,command):
    remaining=budget-cleanup-(time.monotonic()-began);assert remaining>0
    remaining=min(remaining,15 if '-case-' in name else 180)
    before=len(list(Path('/proc/self/fd').iterdir()));timed=False;pidfd=None
    (records/(name+'.command.json')).write_text(json.dumps(command)+'\n')
    with (records/(name+'.stdout')).open('xb') as out,(records/(name+'.stderr')).open('xb') as err:
        p=subprocess.Popen(command,cwd=source,env=env,stdout=out,stderr=err,start_new_session=True,preexec_fn=child_limit)
        try:
            pidfd=os.pidfd_open(p.pid);poll=select.poll();poll.register(pidfd,select.POLLIN)
            ready=poll.poll(max(0,int(remaining*1000)))
            if not ready:
                timed=True;os.killpg(p.pid,signal.SIGKILL)
            code=p.wait(timeout=cleanup)
            reaped=True
        finally:
            if p.poll() is None:
                os.killpg(p.pid,signal.SIGKILL);p.wait(timeout=cleanup)
            if pidfd is not None:os.close(pidfd)
    after=len(list(Path('/proc/self/fd').iterdir()))
    result={'command':command,'exit':code,'timeout':timed,'direct_child_wait':reaped,'fd_before':before,'fd_after':after,
            'descendant_population':'Unverified','elapsed_scope':time.monotonic()-began}
    (records/(name+'.json')).write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps({'stage':name,**result}),flush=True)
    assert before==after
    return code if not timed else 124

def headings(text):
    return sorted(f"{m.group(1)}: {m.group(3)} @ {m.group(4)}" for m in
                  re.finditer(r'^(error|warning)(\[[^\]]+\])?: (.*)\n\s*--> ([^\n]+)', text, re.M))

RUN='crates/init/src/run.rs'
def install(variant):
    pin=spec['variants'][variant];data=base64.b64decode(pin['base64'],validate=True)
    assert len(data)==pin['bytes'] and hashlib.sha256(data).hexdigest()==pin['sha256']
    (source/RUN).write_bytes(data)
    assert hashlib.sha256((source/RUN).read_bytes()).hexdigest()==pin['sha256']
    return pin['sha256']

NAMES=['run::startup_custody_tests::member_start_refusals_report_system_error_and_not_run',
       'run::startup_custody_tests::rejected_parent_group_survives_system_error_until_owner_drop']
REFUSAL='typed member refusal must answer SYSTEM_ERR with NOT_RUN'
EXPECT={'green-v2':[(0,[]),(0,[])],
        'red-v10d':[(101,[REFUSAL,'left: [None, None, None, None, None]']),(101,[REFUSAL,'left: None','right: Some(125)'])],
        'red-v1':[(101,[REFUSAL,'left: [Some(125), Some(125), None, None, None]']),(0,[])],
        'mutant-v2':[(0,[]),(101,['rejected group was released before its startup failure','left: Ok(0)'])]}
TEST=['cargo','test','--locked','--offline','-p','shards-init','--bin','shards-init','--no-run','--message-format=json','--target','x86_64-unknown-linux-gnu','-j2']

def cases(variant):
    run_rs=install(variant)
    code=run(variant+'-test-compile',TEST)
    if code:return {'variant':variant,'run_rs_sha256':run_rs,'compile_exit':code},False
    artifacts=[]
    for line in (records/(variant+'-test-compile.stdout')).read_text().splitlines():
        message=json.loads(line)
        if message.get('reason')=='compiler-artifact' and message.get('profile',{}).get('test') and message.get('executable'):
            artifacts.append(message['executable'])
    assert len(artifacts)==1,artifacts
    executable=Path(artifacts[0]);assert executable.is_relative_to(scope/'target')
    elf=executable.read_bytes();assert elf.startswith(b'\x7fELF')
    binary={'variant':variant,'run_rs_sha256':run_rs,'path':str(executable),'bytes':len(elf),'sha256':hashlib.sha256(elf).hexdigest(),'before_first_case':True}
    (records/(variant+'-precase-binary.json')).write_text(json.dumps(binary,indent=2)+'\n')
    code=run(variant+'-test-list',[str(executable),'--list'])
    if code:return {'variant':variant,'binary':binary,'list_exit':code},False
    listing=(records/(variant+'-test-list.stdout')).read_text()
    assert all(name+': test' in listing for name in NAMES)
    results=[];matched_all=True
    for i,(name,(want,needles)) in enumerate(zip(NAMES,EXPECT[variant])):
        actual=run(f'{variant}-case-{i}',[str(executable),'--exact',name,'--nocapture'])
        stdout=(records/f'{variant}-case-{i}.stdout').read_text(errors='replace')
        stderr=(records/f'{variant}-case-{i}.stderr').read_text(errors='replace')
        summary='1 passed; 0 failed; 0 ignored' if want==0 else '0 passed; 1 failed; 0 ignored'
        matched=actual==want and summary in stdout and all(n in stderr for n in needles)
        results.append({'case':name,'exit':actual,'expected_exit':want,'expected_stderr':needles,'expected_summary':summary,'matched':matched})
        matched_all=matched_all and matched
    assert executable.read_bytes()==elf
    (records/(variant+'-binary-after.json')).write_text(json.dumps({'same_binary':True,**binary},indent=2)+'\n')
    return {'variant':variant,'binary':binary,'cases':results,'matched':matched_all},matched_all

outcomes=[];unexpected=None
try:
    for name,command in [
        ('rustc',['rustc','--version','--verbose']),('cargo',['cargo','--version']),
        ('policy',['python3','scripts/check-development-policy']),
        ('consumer-format',['rustfmt','--check','--edition','2024','--config','skip_children=true']+['crates/init/src/managed/native.rs','crates/init/src/managed/native/storage.rs','crates/init/src/managed/native/jobs.rs','crates/init/src/managed/native/tests.rs','crates/init/src/managed.rs','crates/init/src/domains.rs','crates/init/src/run.rs','crates/init/src/domains/audit.rs','crates/ipc/src/locked_region.rs','crates/ipc/src/locked_region/tests.rs','crates/ipc/src/lib.rs']),
        ('actual-init-check',['cargo','check','--locked','--offline','-p','shards-init','--all-targets','--target','x86_64-unknown-linux-gnu','-j2'])]:
        code=run(name,command);outcomes.append({'stage':name,'exit':code})
        if code:unexpected={'stage':name,'exit':code};break
    if unexpected is None:
        code=run('actual-init-strict',['cargo','clippy','--locked','--offline','-p','shards-init','--all-targets','--target','x86_64-unknown-linux-gnu','-j2','--','-D','warnings'])
        found=headings((records/'actual-init-strict.stderr').read_text(errors='replace'))
        same=found==spec['strict_baseline']
        strict={'stage':'actual-init-strict','exit':code,'diagnostics':len(found),'identical_to_r8_baseline':same,
                'only_new':sorted(set(found)-set(spec['strict_baseline'])),'only_baseline':sorted(set(spec['strict_baseline'])-set(found)),
                'classification':'UNCHANGED known red (unconsumed input/RAW APIs), not a pass' if code==101 and same else 'UNEXPECTED'}
        (records/'strict-classification.json').write_text(json.dumps(strict,indent=2)+'\n');outcomes.append(strict)
        if not (code==101 and same):unexpected=strict
    if unexpected is None:
        for variant in ['green-v2','red-v10d','red-v1','mutant-v2']:
            result,ok=cases(variant);outcomes.append(result)
            if not ok:unexpected=result;break
finally:
    restored=install('green-v2')
code=0 if unexpected is None else 1
(records/'expectations.json').write_text(json.dumps({'all_matched':unexpected is None,'first_unexpected':unexpected,'outcomes':outcomes,'restored_green_run_rs':restored},indent=2)+'\n')
result={'exit':code,'elapsed':time.monotonic()-began,'source_package':manifest['meaning'],'runtime_cases':sum(len(o.get('cases',[])) for o in outcomes),'runtime_scope':'TWO ROOT presentation/actual rejected descriptor unit cases on GREEN-v2, RED-v10d, RED-v1 and MUTANT-v2 only; ZERO helper/actor/nativechild/VM/cgroup/held audit graph/five; no grant/backing or current-main inference','absolute_deadline':deadline,'cleanup_seconds':cleanup}
(records/'final.json').write_text(json.dumps(result,indent=2)+'\n')
for name,pin in manifest['files'].items():
    p=source/name;d=os.readlink(p).encode() if p.is_symlink() else p.read_bytes()
    assert len(d)==pin['bytes'] and hashlib.sha256(d).hexdigest()==pin['sha256'],name
post={'source_entries':len(manifest['files']),'source_manifest_sha256':hashlib.sha256((source/'FROZEN-SOURCE.json').read_bytes()).hexdigest(),'all_source_hashes_unchanged':True,'final_run_rs':'GREEN restored'}
(records/'postchecks-source-binary.json').write_text(json.dumps(post,indent=2)+'\n')
assert template_before==tree_manifest(template/'source')
assert cache_before=={name:tree_manifest(template/'cargo-home'/name) for name in ('registry','git')}
(records/'template-cache-postcheck.json').write_text(json.dumps({'template_files':len(template_before),'cache_files':{k:len(v) for k,v in cache_before.items()},'unchanged':True},indent=2)+'\n')
files={p.relative_to(records).as_posix():{'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in sorted(records.rglob('*')) if p.is_file()}
(records/'MANIFEST.json').write_text(json.dumps(files,indent=2)+'\n')
print(json.dumps(result),flush=True)
sys.exit(code)
