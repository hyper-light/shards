from pathlib import Path
import os,hashlib,io,json,tarfile,gzip,base64
root=Path.home()/'root-managed-limits-a84e1a81fc9a-v1'
assert root.is_dir()
items=[]
for current,dirs,names in os.walk(root,followlinks=False):
 p=Path(current)
 if p==root: dirs[:]=[x for x in dirs if x not in {'frozen','guests','tmp','runtime-cache'}]
 for name in names:
  f=p/name; assert not f.is_symlink(); assert f.is_file()
  items.append((f.relative_to(root).as_posix(),f.read_bytes()))
items.sort()
manifest={'label':'after-a84','source_directory':str(root),'excluded_retained_directories':['frozen','guests','tmp','runtime-cache'],'files':[{'path':n,'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()} for n,b in items],'management_layer_complete':False}
m=json.dumps(manifest,indent=2).encode()+b'\n'
buf=io.BytesIO()
with tarfile.open(fileobj=buf,mode='w') as tar:
 for n,b in items+[('member-hashes.json',m)]:
  member=tarfile.TarInfo(n);member.size=len(b);member.mode=0o600;tar.addfile(member,io.BytesIO(b))
blob=gzip.compress(buf.getvalue(),mtime=0)
result={'bytes':len(blob),'sha256':hashlib.sha256(blob).hexdigest(),'payloads':len(items),'members':len(items)+1,'read_only_generation':True}
import sys
if len(sys.argv) == 3:
    offset, count = map(int, sys.argv[1:])
    assert offset >= 0 and count > 0
    result['offset'] = offset
    result['chunk_b64'] = base64.b64encode(blob[offset:offset+count]).decode()
print(json.dumps(result))
