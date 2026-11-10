python3 - <<'PY'
from pathlib import Path
import os,sys,subprocess,resource,json,hashlib,gzip,base64,shutil
root=Path.home()/'relay-tcp-engine-after-3bbd-v1'
root.mkdir(exist_ok=False)
if shutil.disk_usage(root).free < 20*1024**3: raise RuntimeError('20GiB private disk admission')
record=root/'record';record.mkdir()
os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:4]))
resource.setrlimit(resource.RLIMIT_AS,(8*1024**3,8*1024**3))
source=root/'source'
clone=['git','clone','--filter=blob:none','--single-branch','--branch','p/managed-tcp-forward-impl','git@github.com:hyper-light/shards.git',str(source)]
with (record/'clone.log').open('w') as log: subprocess.run(clone,stdout=log,stderr=subprocess.STDOUT,timeout=180,check=True)
revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
if revision!='3bbdd61058b26979c938aabc35ac86cc845ca0c5': raise RuntimeError('exact signed checkpoint required')
overrides=[]
for f in overrides:
 p=Path(f['path']);data=f['text'].encode()
 if p.is_absolute() or '..' in p.parts or hashlib.sha256(data).hexdigest()!=f['sha256']:raise RuntimeError('source identity')
 out=source/p;out.parent.mkdir(parents=True,exist_ok=True);out.write_bytes(data)
# Parent host integration is only a NEW private test root, never the shared parent.
parent=source/'crates/shards/tests/managed_network.rs'
probe=source/'crates/shards/tests/__relay_tcp.rs'
probe.write_bytes(parent.read_bytes()+b'\n#[path = "managed_network/tcp_forward.rs"]\nmod tcp_forward;\n')
env=os.environ.copy()
frozen_init=Path.home()/'relay-tcp-engine-consumer-2586-guest-v1/artifacts/shards-init'
if hashlib.sha256(frozen_init.read_bytes()).hexdigest()!='af0e18735cf7b76f0a3196ed17ddfbfda045000279e967d777b96c7102684162':raise RuntimeError('source-matched normal init binary')
if hashlib.sha256((source/'crates/init/src/engine.rs').read_bytes()).hexdigest()!='77579f5d2ee2f1a465727960aa67e925a2d5051685232b9066be518894c6a8d1':raise RuntimeError('source-matched engine')
if hashlib.sha256((source/'crates/init/src/relay.rs').read_bytes()).hexdigest()!='d1ce87f9f44dca7a2ea9cfb3c2077e2d6fb82796ccd6fb4548374e891f5e96ff':raise RuntimeError('source-matched shared pump')
env['SHARDS_INIT_BINARY']=str(frozen_init)
env.update({'CARGO_HOME':str(root/'cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'2','NUM_JOBS':'2','CARGO_INCREMENTAL':'0','CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER':'rust-lld','ZIG_TARGET':'x86_64-linux-musl'})
for target in ['x86_64_unknown_linux_gnu','x86_64_unknown_linux_musl']:
 env['CC_'+target]=str(source/'scripts/zig-cc');env['AR_'+target]=str(source/'scripts/zig-ar')
kernel=Path.home()/'managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy'
if hashlib.sha256(kernel.read_bytes()).hexdigest()!='97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae':raise RuntimeError('kernel identity')
env['SHARDS_KERNEL']=str(kernel);env['SHARDS_TEST_KERNEL']=str(kernel)
for f in overrides:
 if f['path'].endswith('.rs'):subprocess.run(['rustup','run','1.98.1','rustfmt','--edition','2024','--config','skip_children=true',str(source/f['path'])],cwd=source,check=True)
identity={'base':revision,'frozen_init':str(frozen_init),'frozen_init_sha256':hashlib.sha256(frozen_init.read_bytes()).hexdigest(),'engine_sha256':hashlib.sha256((source/'crates/init/src/engine.rs').read_bytes()).hexdigest(),'source':[{'path':str(p.relative_to(source)),'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in [source/f['path'] for f in overrides]+[probe,parent,source/'crates/init/src/relay.rs']],'cpu_affinity':sorted(os.sched_getaffinity(0)),'memory_bytes':8*1024**3,'scope':'compile normal source-matching shared init engine consumer plus finite owned probes; no socket or VM case in this build'}
(record/'after-build-input.json').write_text(json.dumps(identity,indent=2))
commands=[('host',['rustup','run','1.98.1','cargo','test','-p','shards','--test','__relay_tcp','--profile','test-release','--no-run','-j','2']),
 ('guest',['rustup','run','1.98.1','cargo','build','-p','shards-testguest','--profile','guest','--target','x86_64-unknown-linux-musl','-j','2'])]
results=[]
for name,args in commands:
 log=record/(name+'-after-build.log')
 with log.open('w') as out:status=subprocess.run(args,cwd=source,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=1800,check=False).returncode
 result={'name':name,'status':status,'command':args,'log':str(log),'sha256':hashlib.sha256(log.read_bytes()).hexdigest()};results.append(result);print(json.dumps(result),flush=True)
 if status:print(log.read_text(errors='replace')[-9000:],flush=True);break
(record/'after-build-result.json').write_text(json.dumps(results,indent=2))
sys.exit(0 if len(results)==2 and all(r['status']==0 for r in results) else 1)
PY
