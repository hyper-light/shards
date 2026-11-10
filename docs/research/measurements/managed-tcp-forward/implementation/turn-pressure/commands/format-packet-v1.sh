python3 - <<'PY'
from pathlib import Path
import os,subprocess,json,hashlib,base64,select
root=Path.home()/'relay-turn-pressure-format-2300-v1';root.mkdir(exist_ok=False)
p=root/'turn_native.rs';p.write_text(os.environ['TURN_NATIVE_SOURCE']);(root/'original.rs').write_bytes(p.read_bytes())
child=subprocess.Popen(['rustup','run','1.98.1','rustfmt','--edition','2024',str(p)],stdout=subprocess.PIPE,stderr=subprocess.PIPE);pidfd=os.pidfd_open(child.pid,0)
try:
 stdout,stderr=child.communicate(timeout=45);poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
finally:
 if child.returncode is None:child.kill();child.wait()
 os.close(pidfd)
(root/'stdout').write_bytes(stdout);(root/'stderr').write_bytes(stderr)
b=p.read_bytes();result={'status':child.returncode,'pidfd_exit_ready':ready,'reaped':child.returncode is not None,'source_sha256':hashlib.sha256(b).hexdigest(),'bytes':len(b),'base64':base64.b64encode(b).decode()}
(root/'result.json').write_text(json.dumps({k:v for k,v in result.items() if k!='base64'},indent=2));print(json.dumps(result))
PY
