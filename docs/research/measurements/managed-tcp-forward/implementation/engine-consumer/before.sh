python3 - <<'PY'
from pathlib import Path
import os,sys,subprocess,resource,json,hashlib,gzip,base64,shutil
root=Path.home()/'relay-tcp-engine-consumer-c080-light-v1'
if shutil.disk_usage(Path.home()).free < 2*1024**3: raise RuntimeError('2GiB private disk floor')
root.mkdir(exist_ok=False); record=root/'record'; record.mkdir()
os.sched_setaffinity(0,{sorted(os.sched_getaffinity(0))[0]})
resource.setrlimit(resource.RLIMIT_AS,(1024**3,1024**3))
source=root/'source'
with (record/'clone.log').open('w') as log:
 subprocess.run(['git','clone','--filter=blob:none','--single-branch','--branch','p/managed-tcp-forward-impl','git@github.com:hyper-light/shards.git',str(source)],stdout=log,stderr=subprocess.STDOUT,timeout=180,check=True)
revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
if revision!='c0807cac94cfb7dd291c82ffab88bdd00b656bfb': raise RuntimeError('exact signed checkpoint required')
files=json.loads(gzip.decompress(base64.b64decode(os.environ['TCP_ENGINE_CONSUMER_PACKET'])))
for f in files:
 p=Path(f['path']);data=f['text'].encode()
 if str(p)!='crates/init/src/engine.rs' or hashlib.sha256(data).hexdigest()!=f['sha256']:raise RuntimeError('owned source identity')
 (source/p).write_bytes(data)
 subprocess.run(['rustup','run','1.98.1','rustfmt','--edition','2024','--config','skip_children=true',str(source/p)],cwd=source,check=True)
paths=['crates/init/src/engine.rs','crates/init/src/relay.rs']
(record/'identity.json').write_text(json.dumps({'base':revision,'policy_revision':'cbd39284937d024640c56db6f8d8ef2bd9b0ee24','rules_sha256':hashlib.sha256((source/'docs/development-rules.md').read_bytes()).hexdigest(),'source':[{'path':p,'sha256':hashlib.sha256((source/p).read_bytes()).hexdigest()} for p in paths],'scope':'normal init build/clippy only; no native actor or VM launch','cpu_affinity':sorted(os.sched_getaffinity(0))},indent=2))
env=os.environ.copy();env.update({'CARGO_HOME':str(Path.home()/'relay-tcp-engine-compat-6b8-v1/cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'1','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld','NUM_JOBS':'1'})
commands=[('policy',['python3','scripts/check-development-policy']),('build',['rustup','run','1.98.1','cargo','build','-p','shards-init','--profile','guest','--target','x86_64-unknown-linux-musl','-j','1']),('clippy',['rustup','run','1.98.1','cargo','clippy','-p','shards-init','--bin','shards-init','--target','x86_64-unknown-linux-musl','-j','1','--','-D','warnings'])]
results=[]
for name,args in commands:
 log=record/(name+'.log')
 with log.open('w') as out:status=subprocess.run(args,cwd=source,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=900,check=False).returncode
 row={'name':name,'status':status,'command':args,'log':str(log),'sha256':hashlib.sha256(log.read_bytes()).hexdigest()};results.append(row);print(json.dumps(row),flush=True)
 if status:print(log.read_text(errors='replace')[-16000:],flush=True)
(record/'result.json').write_text(json.dumps(results,indent=2))
sys.exit(0 if all(r['status']==0 for r in results) else 1)
PY
