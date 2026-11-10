from pathlib import Path
import os,sys,subprocess,resource,json,hashlib,select,time,signal,shutil
root=Path.home()/'relay-tcp-engine-compat-6b8-v1'
case_root=root/'engine-before-v5-normal';case_root.mkdir(exist_ok=False)
if shutil.disk_usage(root).free < 24*1024**3:raise RuntimeError('private disk envelope')
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]})

source=root/'source';target=root/'target'
if hashlib.sha256((source/'crates/init/src/engine.rs').read_bytes()).hexdigest()!='e2e4e6e6ae014ddb82aba96362801bd14bf4a69a2955ac44941bd481f6563eea':raise RuntimeError('old engine body changed')
bins=[p for p in (target/'test-release/deps').glob('__relay_tcp-*') if p.is_file() and os.access(p,os.X_OK)]
if len(bins)!=1:raise RuntimeError('one exact normal host fixture binary')
binary=bins[0];binary_hash=hashlib.sha256(binary.read_bytes()).hexdigest()
guest=target/'x86_64-unknown-linux-musl/guest/shards-testguest'
if not guest.is_file():raise RuntimeError('normal static guest missing')
prefab=root/'prefabs/guest';prefab.mkdir(parents=True,exist_ok=True)
if (prefab/'shards-testguest').resolve()!=guest.resolve():raise RuntimeError('frozen prefab identity')
kernel=Path.home()/'managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy'
config=kernel.with_name('config-6.18.48-x86_64-managed-tproxy')
identity={'source_base':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'binary':str(binary),'binary_sha256':binary_hash,'guest':str(guest),'guest_sha256':hashlib.sha256(guest.read_bytes()).hexdigest(),'engine_source_sha256':hashlib.sha256((source/'crates/init/src/engine.rs').read_bytes()).hexdigest(),'shared_source_sha256':hashlib.sha256((source/'crates/init/src/relay.rs').read_bytes()).hexdigest(),'kernel_sha256':hashlib.sha256(kernel.read_bytes()).hexdigest(),'config_sha256':hashlib.sha256(config.read_bytes()).hexdigest(),'cpu_affinity':sorted(os.sched_getaffinity(0)),'requested_resource_scope':'ROOT approved one ordinary finitecase: Kiln2CPU/4GiB/24GiB headroom, no RLIMIT_AS (normal sparse nativeRAM/DAX), physical/kernel hard limits unverified'}
(case_root/'identity.json').write_text(json.dumps(identity,indent=2))
results=[]
for mode in ['client-fin-first']:
 case=case_root/mode;case.mkdir()
 tmp=case/'tmp';tmp.mkdir()
 cache=case/'helper-cache';cache.mkdir()
 env=os.environ.copy();env.update({'TMPDIR':str(tmp),'XDG_CACHE_HOME':str(cache),'CARGO_HOME':str(root/'cargo-home'),'CARGO_TARGET_DIR':str(target),'SHARDS_TEST_GUESTS':str(root/'prefabs'),'SHARDS_KERNEL':str(kernel),'SHARDS_TEST_KERNEL':str(kernel),'SHARDS_NETWORK_EFFECTIVE_CONFIG':str(config),'SHARDS_NETWORK_CONFIG_PROVENANCE':'ROOT retained experimental6.18.48 TPROXY kernel/config; no product pin change','SHARDS_NETWORK_RECORDS':str(case/'records'),'SHARDS_NETWORK_BOOT_MS':'120000','SHARDS_NETWORK_CASE_MS':'30000','SHARDS_NETWORK_MEMORY_MIB':'256','SHARDS_NETWORK_CPUS':'1','SHARDS_ENGINE_HALF_MODE':mode,'SHARDS_ENGINE_REQUEST_HEX':b'GET /_ping HTTP/1.1\r\nHost: engine\r\nConnection: close\r\n\r\n'.hex(),'SHARDS_ENGINE_SUFFIX_HEX':b'OK'.hex(),'SHARDS_ENGINE_LATE_HEX':b'X'.hex() if mode=='host-fin-first' else '', 'SHARDS_ENGINE_RESPONSE_BYTES':'4096'})
 (case/'inputs.json').write_text(json.dumps({k:v for k,v in env.items() if k.startswith('SHARDS_') or k in ['TMPDIR','XDG_CACHE_HOME']},indent=2))
 args=[str(binary),'--exact','tcp_forward::managed_engine_half_close_records_actual_host_finish','--ignored','--nocapture','--test-threads=1']
 before=len(os.listdir('/proc/self/fd'));pidfd=None;child=None;outcome={'mode':mode,'command':args,'fd_before':before,'pidfd_exit_ready':False,'reaped':False,'timeout':False}
 try:
  with (case/'stdout').open('wb') as out,(case/'stderr').open('wb') as err:
   child=subprocess.Popen(args,cwd=source,env=env,stdout=out,stderr=err,start_new_session=True)
   pidfd=os.pidfd_open(child.pid,0);outcome['pid']=child.pid
   poller=select.poll();poller.register(pidfd,select.POLLIN)
   events=poller.poll(240000);outcome['pidfd_exit_ready']=any(fd==pidfd and flags&select.POLLIN for fd,flags in events)
   if not outcome['pidfd_exit_ready']:outcome['timeout']=True;raise TimeoutError('caller240s bound')
   outcome['status']=child.wait();outcome['reaped']=True
 except Exception as error:outcome['error']={'type':type(error).__name__,'detail':str(error)}
 finally:
  if child is not None and child.returncode is None:
   os.killpg(child.pid,signal.SIGKILL);outcome['status']=child.wait();outcome['reaped']=True;outcome['forced_owned_group_cleanup']=True
  if pidfd is not None:os.close(pidfd)
 outcome['fd_after']=len(os.listdir('/proc/self/fd'))
 text=(case/'stdout').read_text(errors='replace')
 outcome['one_test_passed']='1 passed; 0 failed;' in text and 'running 1 test' in text
 outcome['binary_unchanged']=hashlib.sha256(binary.read_bytes()).hexdigest()==binary_hash
 outcome['kernel_population_return']='Unknown'
 outcome['passed']=outcome.get('status')==0 and outcome['one_test_passed'] and outcome['pidfd_exit_ready'] and outcome['reaped'] and before==outcome['fd_after'] and outcome['binary_unchanged'] and not outcome['timeout']
 (case/'result.json').write_text(json.dumps(outcome,indent=2));results.append(outcome);print(json.dumps(outcome),flush=True)
 print(text[-9000:],flush=True)
 if not outcome['passed']:break
(case_root/'result.json').write_text(json.dumps(results,indent=2))
sys.exit(0 if len(results)==1 and all(r['passed'] for r in results) else 1)
