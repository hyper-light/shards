set -eu
set -o pipefail
umask 077
candidate="${SHARDS_TENANT_DEPTH_REVISION:?signed expiry candidate required}"
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test -z "$(git status --porcelain)"
folder="$campaign/records/expiry-v1/build"
mkdir -p "$campaign/records/expiry-v1"
mkdir "$folder"
export SHARDS_EXPIRY_BUILD_RECORDS="$folder"
timeout 300 git fetch --depth 1 --no-tags origin "$candidate" > "$folder/fetch.log" 2>&1 9>&-
git switch --detach "$candidate" > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = "$candidate"
python3 - <<'PYSCOPE' > "$folder/scope.json"
from pathlib import Path
import subprocess,hashlib,json,os
base='cba6d1c32c5d27ef34b5cd9b49f95834253d6448'
paths=subprocess.check_output(['git','diff','--name-only',base,'HEAD']).decode().splitlines()
assert all(p=='crates/shards/tests/managed_output/current_tenant_stdio.rs' or p.startswith('docs/research/measurements/managed-tenant-depth/') for p in paths),paths
for p in ['crates/testguest/src/workload.rs','crates/shards/tests/common/managed_capture.rs','crates/shards/tests/managed_output.rs','crates/shards/tests/managed_output/tenant_exit.rs']:
 assert Path(p).read_bytes()==subprocess.check_output(['git','show',base+':'+p]),p
assert hashlib.sha256(Path('docs/development-rules.md').read_bytes()).hexdigest()=='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3'
print(json.dumps({'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'canonical_policy':'0342b85d9c808e3ced041a6c3ef0e9bf70438272','fixture_lineage':base,'changed_paths':paths,'native_parent_parser_unchanged':True,'actors_started':False},indent=2))
PYSCOPE
git rev-parse HEAD > "$folder/source.txt"
git show --format=fuller --no-patch HEAD > "$folder/commit.txt"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$campaign/cache/vmlinux-6.18.48-x86_64-17c3ba205400" SHARDS_TEST_KERNEL="$campaign/cache/vmlinux-6.18.48-x86_64-17c3ba205400"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
scripts/check-development-policy > "$folder/policy.log" 2>&1
git diff --check > "$folder/diff-check.log" 2>&1
set +e
timeout --signal=TERM --kill-after=30s 1200s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > "$folder/compile.jsonl" 2> "$folder/compile.log" 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/compile.exit"
if [ "$code" -ne 0 ]; then tail -40 "$folder/compile.log"; exit "$code"; fi
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl timeout --signal=TERM --kill-after=30s 600s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" > "$folder/testguest.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/testguest.exit"
if [ "$code" -ne 0 ]; then tail -40 "$folder/testguest.log"; exit "$code"; fi
set +e
timeout --signal=TERM --kill-after=30s 600s cargo clippy --locked -j4 --profile test-release -p shards --test managed_output -- -D warnings > "$folder/host-clippy.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/host-clippy.exit"
if [ "$code" -ne 0 ]; then tail -40 "$folder/host-clippy.log"; exit "$code"; fi
python3 - <<'PYFREEZE' > "$folder/artifacts.json"
from pathlib import Path
import json,hashlib,os,shutil
r=Path(os.environ['SHARDS_EXPIRY_BUILD_RECORDS'])
packets=[json.loads(s) for s in (r/'compile.jsonl').read_text().splitlines()]
bins=[v['executable'] for v in packets if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1
(r/'test-binary.txt').write_text(bins[0]+'\n')
dirs=[Path(v['out_dir']) for v in packets if v.get('reason')=='build-script-executed' and any(k=='SHARDS_HELPERS_CARRIED' for k,_ in v.get('env',[]))]
assert len(dirs)==1
paths=[Path(bins[0]),Path(os.environ['CARGO_TARGET_DIR'])/'test-release/shards',Path('target/guest/x86_64-unknown-linux-musl/guest/shards-testguest')]+[dirs[0]/n for n in ['shards-init','shards-server','helpers/shards-vm','helpers/shards-net']]
entries=[{'path':str(p.resolve()),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in paths]
frozen=Path('../frozen')/os.environ['SHARDS_TENANT_DEPTH_REVISION'][:9];frozen.mkdir()
for p,e in zip(paths,entries):
 q=frozen/p.name;shutil.copy2(p,q);assert hashlib.sha256(q.read_bytes()).hexdigest()==e['sha256']
(frozen/'guest').mkdir();shutil.copy2(frozen/'shards-testguest',frozen/'guest/shards-testguest')
(frozen/'manifest.json').write_text(json.dumps(entries,indent=2)+'\n')
previous=json.loads(Path('../frozen/cba6d1c32/manifest.json').read_text())
for e in previous:
 p=Path('../frozen/cba6d1c32')/Path(e['path']).name;assert hashlib.sha256(p.read_bytes()).hexdigest()==e['sha256']
assert entries[2]['sha256']=='a9266f916668f0c960e69aef3e32b6acd2ce55540a3cfdc178de80e1e01d0f15'
print(json.dumps({'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'artifacts':entries,'old_cba_frozen_unchanged':True},indent=2))
PYFREEZE
cat "$folder/artifacts.json"
