from pathlib import Path
import base64,hashlib,io,json,tarfile
home=Path.home()
proofs=['managed-network-presentation-proof-v1','managed-network-presentation-proof-v2','managed-network-presentation-host-v1']
files={};binaries=[]
for name in proofs:
 p=home/name
 for path in p.rglob('*'):
  if not path.is_file() or path.is_symlink():continue
  relative=path.relative_to(p)
  if 'target' in relative.parts:
   if path.suffix=='' and path.name.startswith(('sock_addr_pure-','sock_addr_host_pure-')):
    data=path.read_bytes()
    if data[:4]==b'\x7fELF':binaries.append({'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
   continue
  if relative.parts[0]=='source' or path.name=='fixture.rs':continue
  data=path.read_bytes();assert data[:4]!=b'\x7fELF'
  files[str(Path(name)/relative)]=data
files['pack.py']=Path(__file__).read_bytes()
files['binary-identities.json']=(json.dumps(binaries,indent=2)+'\n').encode()
files['SHA256SUMS.json']=(json.dumps({name:hashlib.sha256(data).hexdigest() for name,data in files.items()},indent=2)+'\n').encode()
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as archive:
 for name,data in sorted(files.items()):
  info=tarfile.TarInfo(name);info.size=len(data);info.mode=0o644;archive.addfile(info,io.BytesIO(data))
data=buffer.getvalue()
(home/'managed-network-presentation-light-v1.tgz').write_bytes(data)
print(json.dumps({'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'entries':len(files),'base64':base64.b64encode(data).decode()}))
