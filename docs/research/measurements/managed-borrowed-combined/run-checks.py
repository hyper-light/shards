#!/usr/bin/env python3
"""Assigned actual Mac library/owned regressions and Windows compilation, no VM."""
from pathlib import Path
import base64
import hashlib
import io
import json
import os
import resource
import shutil
import signal
import subprocess
import sys
import tarfile

build_s, test_s, check_s = map(int, sys.argv[1:])
assert min(build_s, test_s, check_s) > 0
root = Path.home()/'managed-borrowed-combined-51fa-v1'
free_before = shutil.disk_usage(root).free
assert free_before >= 32*1024**3
source = root/'source'
source.mkdir()
data=base64.b64decode((root/'source.tar.gz.b64').read_bytes(), validate=True)
assert hashlib.sha256(data).hexdigest()=='88de55575d66ceb5d385b2c25ba457e2278fd829513b6a019490ae99755ff95e'
(root/'source.tar.gz').write_bytes(data)
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:
    for member in tar.getmembers():
        path=Path(member.name)
        assert member.isfile() and not path.is_absolute() and '..' not in path.parts
        dest=source/path
        dest.parent.mkdir(parents=True,exist_ok=True)
        with dest.open('xb') as output:output.write(tar.extractfile(member).read())
for name, sha in json.loads((source/'source-hashes.json').read_text()).items():
    assert hashlib.sha256((source/name).read_bytes()).hexdigest()==sha,name

env={k:v for k,v in os.environ.items() if not k.startswith('DYLD_')}
env.update(CARGO_HOME=str(root/'cargo-home'), CARGO_TARGET_DIR=str(root/'target'),
           RUSTUP_TOOLCHAIN='1.98.1', CARGO_BUILD_JOBS='1', CARGO_INCREMENTAL='0',
           CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS='1')
assert not Path(env['CARGO_TARGET_DIR']).exists()
results={'candidate':'51fa61cef6c2e0435e8549a6014c1024dc497fc0',
         'source_archive_sha256':hashlib.sha256(data).hexdigest(),
         'source_files_verified':37,'free_before_bytes':free_before,
         'driver_nofile_before':resource.getrlimit(resource.RLIMIT_NOFILE),
         'scope':'Actual API unit library and owned fake-muxer regression; Windows compile only. No VM or Mutex change.'}
(root/'artifacts').mkdir()

def group_observation(pgid):
    observed=subprocess.run(['ps','-axo','pid=,ppid=,pgid=,stat=,command='],
                            stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=check_s)
    rows=[]
    if observed.returncode==0:
        for row in observed.stdout.splitlines():
            parts=row.split(None,4)
            if len(parts)>=4 and parts[2]==str(pgid):rows.append(row)
    return {'ps_exit':observed.returncode,'error':observed.stderr,'rows':rows,
            'limit':'One process-group observation, not a full escaped-descendant/reaping/kernel-return proof.'}

def run(name, command, within):
    (root/f'{name}.command.json').write_text(json.dumps(command)+'\n')
    with (root/f'{name}.log').open('wb') as log:
        child=subprocess.Popen(command,cwd=source,env=env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        timed_out=False
        try:
            code=child.wait(timeout=within)
        except subprocess.TimeoutExpired:
            timed_out=True
            # The still-unreaped leader pins this newly created test process-group ID.
            try:os.killpg(child.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            code=child.wait(timeout=check_s)
    text=(root/f'{name}.log').read_text(errors='replace')
    artifacts=[]
    for line in text.splitlines():
        try:item=json.loads(line)
        except ValueError:continue
        if item.get('reason')=='compiler-artifact' and item.get('executable'):
            path=Path(item['executable'])
            saved=root/'artifacts'/f'{name}-api-unit'
            shutil.copyfile(path,saved)
            artifacts.append({'path':str(saved),'bytes':saved.stat().st_size,
                              'sha256':hashlib.sha256(saved.read_bytes()).hexdigest(),
                              'fresh':item['fresh'],'manifest_path':item['manifest_path']})
    result={'exit':code,'timed_out':timed_out,'outer_seconds':within,
            'summary':[line for line in text.splitlines() if line.startswith(('running ','test result:'))],
            'artifacts':artifacts,'post_exit_group':group_observation(child.pid)}
    results[name]=result
    (root/'results.json').write_text(json.dumps(results,indent=2)+'\n')
    print(name,json.dumps(result),flush=True)

base=['cargo','test','--locked','-j1','--profile','test-release','--message-format=json',
      '-p','shards-api','--lib']
run('full-library',base+['--','--test-threads=1','--nocapture'],build_s+test_s)
for name, test in [('owned-zero','client::tests::a_patience_of_zero_dials_once'),
                   ('owned-max','client::tests::a_patience_of_max_dials_until_the_guest_listens')]:
    run(name,base+[test,'--','--exact','--test-threads=1','--nocapture'],test_s)
run('native-lib-clippy',['cargo','clippy','--locked','-j1','-p','shards-api','--lib','--','-D','warnings'],check_s)
installed=(root/'installed-targets.txt').read_text().splitlines()
if 'x86_64-pc-windows-msvc' in installed:
    run('windows-check',['cargo','check','--locked','-j1','-p','shards-api','--all-targets',
                         '--target','x86_64-pc-windows-msvc'],check_s)
    run('windows-lib-clippy',['cargo','clippy','--locked','-j1','-p','shards-api','--lib',
                              '--target','x86_64-pc-windows-msvc','--','-D','warnings'],check_s)
else:
    results['windows-check']={'unrun':'Pinned target not installed; no toolchain mutation.'}
results['free_after_bytes']=shutil.disk_usage(root).free
results['driver_nofile_after']=resource.getrlimit(resource.RLIMIT_NOFILE)
(root/'results.json').write_text(json.dumps(results,indent=2)+'\n')
with (root/'storage.txt').open('w') as output:
    subprocess.run(['du','-sk',str(root)],stdout=output,check=True)
print('FINAL',json.dumps(results),flush=True)
assert all(item.get('exit')==0 for item in results.values() if isinstance(item,dict) and 'exit' in item)
