from pathlib import Path
import base64,hashlib,io,json,tarfile
p=Path.home()/'managed-network-sock-addr-proof-v7'
files={}
for name in ['0.log','1.log','2.log','3.log','4.log','0.command.json','1.command.json','2.command.json','3.command.json','4.command.json','results.json','source-sha256.json','helper/Cargo.toml','helper/Cargo.lock','helper/lib.rs','devcgroup/Cargo.toml','pack.py']:
 files[name]=(p/name).read_bytes()
binaries=[]
for pattern in ['target/debug/deps/sock_addr_pure-*','target/x86_64-unknown-linux-musl/debug/deps/sock_addr_pure-*']:
 for binary in p.glob(pattern):
  if binary.suffix=='':
   data=binary.read_bytes()
   if data[:4]==b'\x7fELF':binaries.append({'path':str(binary),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
files['binary-identities.json']=(json.dumps(binaries,indent=2)+'\n').encode()
files['SHA256SUMS.json']=(json.dumps({name:hashlib.sha256(data).hexdigest() for name,data in files.items()},indent=2)+'\n').encode()
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as archive:
 for name,data in sorted(files.items()):
  info=tarfile.TarInfo(name);info.size=len(data);info.mode=0o644;archive.addfile(info,io.BytesIO(data))
data=buffer.getvalue()
(Path.home()/'managed-network-sock-addr-setns-light-v1.tgz').write_bytes(data)
print(json.dumps({'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'entries':len(files),'base64':base64.b64encode(data).decode()}))
