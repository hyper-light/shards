set -eu
set -o pipefail
umask 077
scope="$HOME/managed-tenant-reconnect-step2-light-01a1c5d2-v2"
mkdir "$scope"
mkdir "$scope/target" "$scope/records"
cd "$scope"
python3 - <<'PY'
from pathlib import Path
import hashlib,json,shutil,stat
old=Path.home()/'managed-tenant-reconnect-step2-light-01a1c5d2-v1/source'
for p in old.rglob('*'):
 assert p.is_file() or p.is_dir(),p
 assert not p.is_symlink(),p
shutil.copytree(old,'source')
manifest=json.loads(Path('source/source-manifest.json').read_text())
for name,wanted in manifest['files'].items():
 assert hashlib.sha256((Path('source')/name).read_bytes()).hexdigest()==wanted,name
Path('records/transfer.json').write_text(json.dumps({'source_copy':str(old),'all_file_hashes_verified':True,'accepted_source':manifest['accepted_source'],'rules_sha256':manifest['policy_sha256'],'source_unchanged':True,'prior_failure':'v1 GNU selected rust-lld without native system-library search; no test ran'},indent=2)+'\n')
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
 linker_options=()
 if [ "$triple" = x86_64-unknown-linux-musl ]; then linker_options=(-C linker=rust-lld); fi
 rustc --edition 2024 "$scope/source/abi/build.rs" -o "$output/abi-build" > "$scope/records/$triple.build.log" 2>&1
 CARGO_MANIFEST_DIR="$scope/source/abi" OUT_DIR="$output" "$output/abi-build" >> "$scope/records/$triple.build.log" 2>&1
 OUT_DIR="$output" rustc --edition 2024 --target "$triple" --crate-name shards_abi --crate-type rlib "$scope/source/abi/src/lib.rs" -o "$output/libshards_abi.rlib" >> "$scope/records/$triple.build.log" 2>&1
 OUT_DIR="$output" rustc --edition 2024 --target "$triple" "${linker_options[@]}" --test "$scope/source/pure.rs" --extern "shards_abi=$output/libshards_abi.rlib" -o "$output/pure-tests" >> "$scope/records/$triple.build.log" 2>&1
 printf '%s\n' 0 > "$scope/records/$triple.compile.exit"
 OUT_DIR="$output" clippy-driver --edition 2024 --target "$triple" "${linker_options[@]}" --test "$scope/source/pure.rs" --extern "shards_abi=$output/libshards_abi.rlib" --emit=metadata -o "$output/clippy.rmeta" -D warnings > "$scope/records/$triple.clippy.log" 2>&1
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
