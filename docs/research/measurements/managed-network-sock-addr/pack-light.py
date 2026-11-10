#!/usr/bin/env python3
"""Retain compiler reds/greens and source identities; exclude executable bytes."""
import hashlib
import io
import json
from pathlib import Path
import tarfile

home=Path.home()
names=['managed-network-sock-addr-proof-v'+str(n) for n in range(1,6)]
names+=['managed-network-sock-addr-host-red-v1','managed-network-sock-addr-host-red-v2','managed-network-sock-addr-host-green-v1']
files={}
binaries=[]
for name in names:
 p=home/name
 assert p.is_dir(),p
 for source in sorted(p.rglob('*')):
  if not source.is_file() or source.is_symlink(): continue
  relative=source.relative_to(p)
  if 'target' in relative.parts:
   if source.name.startswith(('sock_addr_pure-','sock_addr_host_pure-')) and source.suffix=='':
    data=source.read_bytes()
    if data[:4]==b'\x7fELF': binaries.append({'path':str(source),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
   continue
  if relative.parts[0]=='source' and source.name not in {'sock_addr.rs','bpf.rs'}: continue
  if source.name=='fixture.rs': continue # Exact host snapshots identified by signed commits + hash.
  data=source.read_bytes()
  assert data[:4]!=b'\x7fELF'
  files[str(Path(name)/relative)]=data
files['binary-identities.json']=(json.dumps(binaries,indent=2)+'\n').encode()
manifest={name:hashlib.sha256(data).hexdigest() for name,data in files.items()}
files['SHA256SUMS.json']=(json.dumps(manifest,indent=2)+'\n').encode()
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as archive:
 for name,data in sorted(files.items()):
  info=tarfile.TarInfo(name);info.size=len(data);info.mode=0o644
  archive.addfile(info,io.BytesIO(data))
data=buffer.getvalue()
(home/'managed-network-sock-addr-light-bundle-v2.tgz').write_bytes(data)
print(json.dumps({'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'entries':len(files),'binaries':binaries}))
