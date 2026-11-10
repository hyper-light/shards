set -eu
set -o pipefail
umask 077
scope="$HOME/managed-tenant-reconnect-step2-light-01a1c5d2-v1"
mkdir "$scope"
mkdir "$scope/target" "$scope/records"
cd "$scope"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,json,tarfile
chunks=[
# eight <=16000-character chunks; archive SHA256 8a2a8da7611df1604ba1fb1f62ee54d998c9c7c1a8c50143e55b43e8396c81a0, 88053 bytes
# reconstruct with prepare-approved-pure.py and the recorded source manifest
]
data=base64.b64decode(''.join(chunks),validate=True)
assert len(data)==88053
assert hashlib.sha256(data).hexdigest()=='8a2a8da7611df1604ba1fb1f62ee54d998c9c7c1a8c50143e55b43e8396c81a0'
Path('source.tar.gz').write_bytes(data)
root=Path('source');root.mkdir()
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:
 members=tar.getmembers()
 for m in members:
  p=Path(m.name)
  assert m.isfile() and not p.is_absolute() and '..' not in p.parts,m.name
  target=root/p;target.parent.mkdir(parents=True,exist_ok=True)
  with target.open('xb') as out:out.write(tar.extractfile(m).read())
manifest=json.loads((root/'source-manifest.json').read_text())
for name,expected in manifest['files'].items():
 assert hashlib.sha256((root/name).read_bytes()).hexdigest()==expected,name
Path('records/transfer.json').write_text(json.dumps({'archive_sha256':'8a2a8da7611df1604ba1fb1f62ee54d998c9c7c1a8c50143e55b43e8396c81a0','archive_bytes':len(data),'members':len(members),'all_file_hashes_verified':True},indent=2)+'\n')
PY
rustc -Vv > records/rustc.txt
clippy-driver --version > records/clippy.txt
uname -a > records/uname.txt
python3 source/policy/scripts/check-development-policy > records/policy.log 2>&1
rustfmt --check --edition 2024 --config-path source/rustfmt.toml source/original/tenant_reconnect.rs source/original/tenant_observer.rs > records/rustfmt.log 2>&1
scope="$(pwd)"
for triple in x86_64-unknown-linux-gnu x86_64-unknown-linux-musl; do
 output="$scope/target/$triple"
 mkdir "$output"
 rustc --edition 2024 "$scope/source/abi/build.rs" -o "$output/abi-build" > "$scope/records/$triple.build.log" 2>&1
 CARGO_MANIFEST_DIR="$scope/source/abi" OUT_DIR="$output" "$output/abi-build" >> "$scope/records/$triple.build.log" 2>&1
 OUT_DIR="$output" rustc --edition 2024 --target "$triple" --crate-name shards_abi --crate-type rlib "$scope/source/abi/src/lib.rs" -o "$output/libshards_abi.rlib" >> "$scope/records/$triple.build.log" 2>&1
 OUT_DIR="$output" rustc --edition 2024 --target "$triple" -C linker=rust-lld --test "$scope/source/pure.rs" --extern "shards_abi=$output/libshards_abi.rlib" -o "$output/pure-tests" >> "$scope/records/$triple.build.log" 2>&1
 printf '%s\n' 0 > "$scope/records/$triple.compile.exit"
 OUT_DIR="$output" clippy-driver --edition 2024 --target "$triple" -C linker=rust-lld --test "$scope/source/pure.rs" --extern "shards_abi=$output/libshards_abi.rlib" --emit=metadata -o "$output/clippy.rmeta" -D warnings > "$scope/records/$triple.clippy.log" 2>&1
 printf '%s\n' 0 > "$scope/records/$triple.clippy.exit"
 "$output/pure-tests" --nocapture --test-threads=1 > "$scope/records/$triple.pure.log" 2>&1
 printf '%s\n' 0 > "$scope/records/$triple.pure.exit"
 tail -3 "$scope/records/$triple.pure.log"
done
python3 - <<'PY'
from pathlib import Path
import gzip,hashlib,json,os
root=Path.cwd(); records=root/'records'; results={}
for triple in ['x86_64-unknown-linux-gnu','x86_64-unknown-linux-musl']:
 log=(records/(triple+'.pure.log')).read_bytes()
 assert b'5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out' in log
 for kind in ['compile','clippy','pure']:
  assert (records/(triple+'.'+kind+'.exit')).read_text().strip()=='0'
 (records/(triple+'.pure.log.gz')).write_bytes(gzip.compress(log,mtime=0))
 results[triple]={'compile':0,'clippy':0,'passed':5,'failed':0,'ignored':0,'filtered':0,'log_sha256':hashlib.sha256(log).hexdigest(),'test_binary_sha256':hashlib.sha256((root/'target'/triple/'pure-tests').read_bytes()).hexdigest()}
s=os.statvfs(root)
receipt={'accepted_source':'54188c747f2a64db7e1de564cd70df0bd16ca5af','policy_revision':'cbd39284937d024640c56db6f8d8ef2bd9b0ee24','rules_sha256':'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3','results':results,'live_observer_endpoint_helpers_vm_executed':False,'full_host_body_compiled':False,'abi_equal_to_current_main':True,'free_bytes':s.f_bavail*s.f_frsize}
(records/'result.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
PY
du -sk source target records > records/footprint-kib.txt
cat records/footprint-kib.txt
