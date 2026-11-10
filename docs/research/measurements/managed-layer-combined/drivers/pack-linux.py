from pathlib import Path
import json,hashlib,tarfile,io,subprocess,shutil
h=Path.home();repo=h/'managed-capture-check';after=h/'root-managed-layer-combined-0219-v1'
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()=='0219ab2eb2466ee3e9b08064434dc9efd70053d5'
assert not subprocess.check_output(['git','status','--porcelain','--untracked-files=no'],cwd=repo)
frozen=after/'frozen';frozen.mkdir(exist_ok=False)
selected=json.loads((after/'compiler-artifact.json').read_text())['executable']
files=[Path(selected),repo/'target/test-release/shards',after/'guests/guest/shards-testguest']
facts=[]
for i,p in enumerate(files):
 assert p.is_file();q=frozen/(str(i)+'-'+p.name);shutil.copy2(p,q)
 with q.open('rb') as f:sha=hashlib.file_digest(f,'sha256').hexdigest()
 facts.append({'original':str(p),'frozen':str(q),'bytes':q.stat().st_size,'sha256':sha})
(after/'frozen-associations.json').write_text(json.dumps(facts,indent=2)+'\n')
archive=h/'root-managed-layer-combined-linux-originals-v1.tar.gz';assert not archive.exists();members=[]
with tarfile.open(archive,'w:gz') as t:
 for label,source in [('before-f1a3',h/'root-managed-layer-combined-f1a3-v1'),('after-0219',after),('mutation-0219',h/'root-managed-broker-mutation-0219-v1')]:
  assert source.is_dir()
  for p in sorted(source.rglob('*')):
   rel=p.relative_to(source)
   if any(part in ['frozen','guests','runtime-cache','empty-docker'] for part in rel.parts) or p.is_dir():continue
   assert p.is_file() and not p.is_symlink()
   data=p.read_bytes();assert not data.startswith(bytes.fromhex('7f454c46'))
   name=label+'/'+rel.as_posix();t.add(p,arcname=name,recursive=False)
   members.append({'path':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
 payload=(json.dumps(members,indent=2)+'\n').encode();info=tarfile.TarInfo('regular-members.json');info.size=len(payload);t.addfile(info,io.BytesIO(payload))
print(json.dumps({'path':str(archive),'bytes':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'original_members':len(members),'frozen':facts}))
