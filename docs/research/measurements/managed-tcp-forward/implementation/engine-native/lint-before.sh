python3 - <<'PY'
from pathlib import Path
import os,subprocess,resource,shutil,json,tomllib,hashlib
root=Path.home()/'relay-engine-unix-clippy-v1'
if shutil.disk_usage(Path.home()).free<2*1024**3:raise RuntimeError('2GiB private floor')
root.mkdir(exist_ok=False)
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]});resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
original=Path.home()/'relay-tcp-forward-cbd3-light-v13'
source=root/'source';check=root/'check';shutil.copytree(original/'source',source);shutil.copytree(original/'check',check)
normal=Path.home()/'relay-tcp-engine-consumer-3bbd-guest-v2/source'
shutil.copy2(normal/'clippy.toml',source/'clippy.toml')
settings=tomllib.loads((normal/'Cargo.toml').read_text())['workspace']['lints']
flags=[]
for group,items in settings.items():
 for name,value in items.items():
  level=value if isinstance(value,str) else value['level']
  flags += ['--'+level,('clippy::' if group=='clippy' else '')+name]
env=os.environ.copy();env.update({'CLIPPY_CONF_DIR':str(source),'CARGO_HOME':str(Path.home()/'relay-tcp-forward-cbd3-light-v4/cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'1','CARGO_INCREMENTAL':'0','NUM_JOBS':'1','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld'})
results=[]
for target in ['x86_64-unknown-linux-musl','x86_64-unknown-linux-gnu']:
 command=['rustup','run','1.98.1','cargo','clippy','--lib','--all-targets','--target',target,'-j','1','--',*flags,'-D','warnings']
 log=root/(target+'.log')
 with log.open('wb') as out:status=subprocess.run(command,cwd=check,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=300,check=False).returncode
 row={'target':target,'status':status,'command':command,'log':str(log),'sha256':hashlib.sha256(log.read_bytes()).hexdigest()};results.append(row);print(json.dumps(row),flush=True)
 if status:print(log.read_text()[-16000:],flush=True)
(root/'result.json').write_text(json.dumps({'scope':'private source projection, not full testguest caller; workspace lint flags and unmodified clippy config','source_sha256':hashlib.sha256((source/'crates/testguest/src/managed_network/intercept/tcp_forward.rs').read_bytes()).hexdigest(),'workspace_lints':settings,'results':results},indent=2))
raise SystemExit(0 if all(r['status']==0 for r in results) else 1)
PY
