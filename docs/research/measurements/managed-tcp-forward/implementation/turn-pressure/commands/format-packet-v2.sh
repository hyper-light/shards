python3 - <<'PY'
from pathlib import Path
import os,subprocess,hashlib,json,select,resource
root=Path.home()/'relay-turn-pressure-format-2300-v2';root.mkdir(exist_ok=False);old=Path.home()/'relay-turn-pressure-2300-v1/source'
resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:2]))
p=root/'turn_native.rs';p.write_bytes((old/'crates/init/src/relay/turn_native.rs').read_bytes());config=root/'rustfmt.toml';config.write_bytes((old/'rustfmt.toml').read_bytes());(root/'original.rs').write_bytes(p.read_bytes())
assert hashlib.sha256(p.read_bytes()).hexdigest()=='2224c87275af12e6a09aa94e6cdd0bbe2076124e4c615761c2e86e203a8928fe'
child=subprocess.Popen(['rustup','run','1.98.1','rustfmt','--edition','2024','--config-path',str(config),str(p)],stdout=subprocess.PIPE,stderr=subprocess.PIPE);pidfd=os.pidfd_open(child.pid,0)
try:
 stdout,stderr=child.communicate(timeout=45);poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
finally:
 if child.returncode is None:child.kill();child.wait()
 os.close(pidfd)
(root/'stdout').write_bytes(stdout);(root/'stderr').write_bytes(stderr)
result={'status':child.returncode,'pidfd_exit_ready':ready,'reaped':child.returncode is not None,'source_sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'source_bytes':p.stat().st_size,'config_sha256':hashlib.sha256(config.read_bytes()).hexdigest()};(root/'result.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))
PY
