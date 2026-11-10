from pathlib import Path
import json,hashlib,tarfile,io
h=Path.home();archive=h/'root-managed-layer-combined-mac-originals-v1.tar.gz';assert not archive.exists();members=[]
with tarfile.open(archive,'w:gz') as t:
 for label,source in [('broker-270bc',h/'root-managed-seven-270bc-v1'),('failed-f1a3',h/'root-managed-seven-f1a3-v1'),('passed-0219',h/'root-managed-seven-0219-v1')]:
  assert source.is_dir()
  for p in sorted(source.rglob('*')):
   if p.is_dir():continue
   assert p.is_file() and not p.is_symlink()
   data=p.read_bytes();name=label+'/'+p.relative_to(source).as_posix();t.add(p,arcname=name,recursive=False);members.append({'path':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
 payload=(json.dumps(members,indent=2)+'\n').encode();info=tarfile.TarInfo('regular-members.json');info.size=len(payload);t.addfile(info,io.BytesIO(payload))
print(json.dumps({'path':str(archive),'bytes':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'original_members':len(members)}))
