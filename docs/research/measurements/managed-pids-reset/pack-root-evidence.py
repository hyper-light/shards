from pathlib import Path
import hashlib,io,json,tarfile,sys
label,folder=sys.argv[1:]
root=Path.home()/folder
assert root.is_dir()
destination=Path.home()/("root-evidence-pack-"+label+"-v1")
destination.mkdir(exist_ok=False)
skip={"frozen","guests","tmp","runtime-cache"}
files=[]
for path in sorted(root.rglob("*")):
    relative=path.relative_to(root)
    if relative.parts[0] in skip:
        continue
    assert not path.is_symlink(),str(path)
    if not path.is_file():
        continue
    files.append((relative.as_posix(),path.read_bytes()))
manifest={"label":label,"source_directory":str(root),"excluded_retained_directories":sorted(skip),"files":[{"path":name,"bytes":len(data),"sha256":hashlib.sha256(data).hexdigest()} for name,data in files],"management_layer_complete":False}
m=json.dumps(manifest,indent=2).encode()+b"\n"
archive=destination/"raw.tar.gz"
with tarfile.open(archive,"w:gz") as tar:
    for name,data in files+[("member-hashes.json",m)]:
        item=tarfile.TarInfo(name);item.size=len(data);item.mode=0o600
        tar.addfile(item,io.BytesIO(data))
blob=archive.read_bytes()
receipt={"label":label,"archive":str(archive),"bytes":len(blob),"sha256":hashlib.sha256(blob).hexdigest(),"payloads":len(files),"members":len(files)+1}
(destination/"receipt.json").write_text(json.dumps(receipt,indent=2)+"\n")
print(json.dumps(receipt))
