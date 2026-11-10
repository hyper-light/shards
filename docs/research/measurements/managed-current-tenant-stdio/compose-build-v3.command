set -eu
set -o pipefail
umask 077
candidate="${SHARDS_CURRENT_TENANT_REVISION:?exact signed composed candidate required}"
campaign="$HOME/managed-current-tenant-28e-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(cat ../records/compile.exit)" = 0
test "$(cat ../records/testguest.exit)" = 0
test -z "$(git status --porcelain)"
folder="$campaign/records/candidate-v3"
mkdir "$folder"
export SHARDS_CURRENT_BUILD_RECORDS="$folder"
timeout 300 git fetch --depth 1 --no-tags origin "$candidate" > "$folder/fetch.log" 2>&1 9>&-
git switch --detach "$candidate" > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
git rev-parse HEAD > "$folder/source.txt"
git show --format=fuller --no-patch HEAD > "$folder/commit.txt"
python3 - <<'PY' > "$folder/source-scope.json"
from pathlib import Path
import os,json,subprocess,hashlib
base='28e147c4dda846ca023020e03bc9b00eb2e58e94'
paths=subprocess.check_output(['git','diff','--name-only',base,'HEAD']).decode().splitlines()
allowed={'crates/testguest/src/workload.rs','crates/shards/tests/managed_output.rs','crates/shards/tests/managed_output/current_tenant_stdio.rs','crates/shards/tests/managed_output/tenant_stdio.rs','crates/shards/tests/managed_output/tenant_exit.rs','crates/shards/tests/managed_output/broker_effects.rs'}
assert all(p in allowed or p.startswith('docs/research/measurements/managed-current-tenant-stdio/') for p in paths),paths
capture='crates/shards/tests/common/managed_capture.rs'
assert Path(capture).read_bytes()==subprocess.check_output(['git','show',base+':'+capture])
policy=hashlib.sha256(Path('docs/development-rules.md').read_bytes()).hexdigest()
assert policy=='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3'
workload=Path('crates/testguest/src/workload.rs').read_text()
assert '"inherit-stdio" =>' in workload and '"inherited-child" =>' in workload
assert 'agent_tasks: Option<u32>' in Path('crates/shards/tests/managed_output.rs').read_text()
print(json.dumps({'base':base,'candidate':os.environ['SHARDS_CURRENT_TENANT_REVISION'],'changed_paths':paths,'shared_capture_unchanged':True,'policy_sha256':policy,'workload_sha256':hashlib.sha256(workload.encode()).hexdigest()},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
scripts/check-development-policy > "$folder/policy.log" 2>&1
git diff --check > "$folder/diff-check.log" 2>&1
set +e
timeout --signal=TERM --kill-after=30s 3600s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > "$folder/compile.jsonl" 2> "$folder/compile.log" 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/compile.exit"
if [ "$code" -ne 0 ]; then tail -60 "$folder/compile.log"; exit "$code"; fi
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl timeout --signal=TERM --kill-after=30s 1800s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" > "$folder/testguest.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/testguest.exit"
if [ "$code" -ne 0 ]; then tail -60 "$folder/testguest.log"; exit "$code"; fi
python3 - <<'PY' > "$folder/artifacts.json"
from pathlib import Path
import json,hashlib,os,shutil
r=Path(os.environ['SHARDS_CURRENT_BUILD_RECORDS'])
packets=[json.loads(line) for line in (r/'compile.jsonl').read_text().splitlines()]
bins=[v['executable'] for v in packets if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1,bins
(r/'test-binary.txt').write_text(bins[0]+'\n')
dirs=[Path(v['out_dir']) for v in packets if v.get('reason')=='build-script-executed' and any(k=='SHARDS_HELPERS_CARRIED' for k,_ in v.get('env',[]))]
assert len(dirs)==1,dirs
paths=[Path(bins[0]),Path(os.environ['CARGO_TARGET_DIR'])/'test-release/shards',Path('target/guest/x86_64-unknown-linux-musl/guest/shards-testguest')]
paths.extend(dirs[0]/n for n in ['shards-init','shards-server','helpers/shards-vm','helpers/shards-net'])
entries=[{'path':str(p.resolve()),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in paths]
assert len(entries)==7 and all(e['bytes']>0 for e in entries)
frozen=Path('../frozen')/os.environ['SHARDS_CURRENT_TENANT_REVISION'][:9];frozen.mkdir()
for p,e in zip(paths,entries):
    q=frozen/p.name;shutil.copy2(p,q)
    assert hashlib.sha256(q.read_bytes()).hexdigest()==e['sha256']
(frozen/'manifest.json').write_text(json.dumps(entries,indent=2)+'\n')
print(json.dumps(entries,indent=2))
PY
du -sk "$campaign/source" "$campaign/target" "$campaign/cache" "$campaign/frozen" "$campaign/records" > "$folder/footprint-kib.txt"
cat "$folder/compile.exit" "$folder/testguest.exit" "$folder/artifacts.json"
