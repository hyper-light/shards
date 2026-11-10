set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test -z "$(git status --porcelain)"
python3 - <<'PY'
from pathlib import Path
import json,shutil,hashlib
root=Path.home()/'managed-tenant-live-01a1c5d2-v1'
destination=root/'frozen/446211e57'
destination.mkdir(parents=True,exist_ok=False)
entries=json.loads((root/'records/artifacts.json').read_text())
for item in entries:
    source=Path(item['path']);target=destination/source.name
    assert hashlib.sha256(source.read_bytes()).hexdigest()==item['sha256']
    shutil.copy2(source,target)
    assert hashlib.sha256(target.read_bytes()).hexdigest()==item['sha256']
(destination/'manifest.json').write_text(json.dumps(entries,indent=2)+'\n')
PY
mkdir "$campaign/records/inherited-v2"
folder="$campaign/records/inherited-v2"
git fetch --depth=1 origin refs/heads/p/managed-tenant-live-cbd-v1:refs/remotes/origin/p/managed-tenant-live-cbd-v1 > "$folder/fetch.log" 2>&1
git switch --detach f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852 > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852
test -z "$(git status --porcelain)"
git rev-parse HEAD > "$folder/source.txt"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852
export SHARDS_TENANT_STDIO_SOURCE_REVISION=f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852
export XDG_CACHE_HOME="$campaign/cache"
export TMPDIR=/tmp/mt-01a1-live-v1
printf '%s\n' 'cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json' > "$folder/compile.command"
set +e
timeout --signal=TERM --kill-after=30s 1800s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > "$folder/compile.jsonl" 2> "$folder/compile.log" 9>&-
compile_status="$?"
set -e
printf '%s\n' "$compile_status" > "$folder/compile.exit"
if [ "$compile_status" -ne 0 ]; then tail -80 "$folder/compile.log"; exit "$compile_status"; fi
python3 - <<'PY' > "$folder/test-binary.txt"
from pathlib import Path
import json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/inherited-v2/compile.jsonl'
bins=[v['executable'] for line in p.read_text().splitlines() if (v:=json.loads(line)).get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1,bins
print(bins[0])
PY
binary="$(cat "$folder/test-binary.txt")"
. ../records/live-inputs.env
export SHARDS_TENANT_STDIO_TASKS=4
export SHARDS_TENANT_STDIO_DEADLINE_MS=300000
export SHARDS_TENANT_STDIO_WORK_MS=300000
export SHARDS_TENANT_STDIO_CLEANUP_MS=300000
export SHARDS_TENANT_STDIO_REPORT_BYTES=269
export SHARDS_TENANT_STDIO_RECORDS="$folder/records"
python3 - <<'PY' > "$folder/environment.json"
import os,json
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith('SHARDS_TENANT_STDIO_') or k in ['SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','CARGO_TARGET_DIR','CARGO_BUILD_JOBS','DOCKER_CONFIG','TMPDIR','XDG_CACHE_HOME']},indent=2))
PY
printf '%s\n' "$binary tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1" > "$folder/command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1 > "$folder/test.log" 2>&1 9>&-
test_status="$?"
set -e
printf '%s\n' "$test_status" > "$folder/test.exit"
tail -80 "$folder/test.log"
if [ -f "$folder/records/history-before-cleanup.json" ]; then cat "$folder/records/history-before-cleanup.json"; fi
if [ -f "$folder/records/teardown.json" ]; then cat "$folder/records/teardown.json"; fi
du -sk "$campaign/source" "$campaign/target" "$campaign/cache" "$campaign/frozen" "$campaign/records" > "$folder/footprint-kib.txt"
exit "$test_status"
