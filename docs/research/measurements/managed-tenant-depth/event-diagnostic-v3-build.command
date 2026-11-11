timeout --signal=TERM --kill-after=15s 600s bash <<'SHARDS_DIAGNOSTIC_BUILD'
set -eu
set -o pipefail
umask 077
candidate=249dc4cf047586bcb040ae7895bc95769cde5b57
campaign="$HOME/managed-tenant-event-diag-01a1c5d2-v2"
cd "$campaign/source"
exec 9>"$campaign/.light.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
python3 - <<'PYGATE'
from pathlib import Path
import json,hashlib
r=Path('../records')
assert json.loads((r/'result.json').read_text())['host_clippy_exit']==0
source=json.loads((r/'source-readback.json').read_text())
assert source['source']=='249dc4cf047586bcb040ae7895bc95769cde5b57'
for p,d in source['files'].items():assert hashlib.sha256(Path(p).read_bytes()).hexdigest()==d,p
assert not list(r.rglob('actor-started.once'))
PYGATE
folder="$campaign/records/build-v1"
mkdir "$folder"
export SHARDS_DIAGNOSTIC_BUILD_RECORDS="$folder"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target" CARGO_HOME="$campaign/cargo-home" TMPDIR="$campaign/tmp"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
rustc -Vv > "$folder/rustc.txt"
cargo -V > "$folder/cargo.txt"
set +e
cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > "$folder/compile.jsonl" 2> "$folder/compile.log" 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/compile.exit"
if [ "$code" -ne 0 ]; then tail -30 "$folder/compile.log"; exit "$code"; fi
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" > "$folder/testguest.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/testguest.exit"
if [ "$code" -ne 0 ]; then tail -30 "$folder/testguest.log"; exit "$code"; fi
python3 - <<'PYFREEZE'
from pathlib import Path
import json,hashlib,os,shutil,subprocess
r=Path(os.environ['SHARDS_DIAGNOSTIC_BUILD_RECORDS'])
messages=[json.loads(s) for s in (r/'compile.jsonl').read_text().splitlines()]
binary=[v['executable'] for v in messages if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(binary)==1
carried=[Path(v['out_dir']) for v in messages if v.get('reason')=='build-script-executed' and any(k=='SHARDS_HELPERS_CARRIED' for k,_ in v.get('env',[]))]
assert len(carried)==1
paths=[Path(binary[0]),Path(os.environ['CARGO_TARGET_DIR'])/'test-release/shards',Path('target/guest/x86_64-unknown-linux-musl/guest/shards-testguest')]+[carried[0]/p for p in ['shards-init','shards-server','helpers/shards-vm','helpers/shards-net']]
entries=[{'path':str(p.resolve()),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in paths]
frozen=Path('../frozen/249dc4cf0-v1');frozen.mkdir(parents=True)
for p,e in zip(paths,entries):
 q=frozen/p.name
 assert not q.exists()
 shutil.copy2(p,q)
 assert hashlib.sha256(q.read_bytes()).hexdigest()==e['sha256']
(frozen/'guest').mkdir();shutil.copy2(frozen/'shards-testguest',frozen/'guest/shards-testguest')
original=json.loads(Path('docs/research/measurements/managed-tenant-depth/raw-expiry-v1/build/artifacts.json').read_text())['artifacts']
expected={Path(e['path']).name:e for e in original[1:]}
comparison=[{'name':Path(e['path']).name,'actual':e,'original':expected[Path(e['path']).name],'equal':e['sha256']==expected[Path(e['path']).name]['sha256'] and e['bytes']==expected[Path(e['path']).name]['bytes']} for e in entries[1:]]
result={'source':subprocess.check_output(['git','rev-parse','HEAD']).decode().strip(),'artifacts':entries,'original_product_comparison':comparison,'all_six_product_native_bytes_match':all(c['equal'] for c in comparison),'vm_executions':0,'runtime_gate':'Hold unless original frozen product bytes all match and exact input/readback is retained'}
(frozen/'manifest.json').write_text(json.dumps(entries,indent=2)+'\n')
(r/'artifacts.json').write_text(json.dumps(result,indent=2)+'\n')
(r/'test-binary.txt').write_text(str(frozen.resolve()/Path(binary[0]).name)+'\n')
print(json.dumps(result,indent=2))
assert result['source']=='249dc4cf047586bcb040ae7895bc95769cde5b57'
assert result['all_six_product_native_bytes_match'],'Original frozen product mismatch; no actor or substitution'
PYFREEZE
SHARDS_DIAGNOSTIC_BUILD
