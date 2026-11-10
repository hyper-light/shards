set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test -z "$(git status --porcelain)"
mkdir "$campaign/frozen/f4ee5f7c5"
cp "$(cat ../records/inherited-v2-live/test-binary.txt)" "$campaign/frozen/f4ee5f7c5/managed_output"
folder="$campaign/records/caption-v4"
mkdir "$folder"
git fetch --depth=1 origin 0ea38a52aa534bf665516c239c4bf122149f8554 > "$folder/fetch.log" 2>&1
git switch --detach 0ea38a52aa534bf665516c239c4bf122149f8554 > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = 0ea38a52aa534bf665516c239c4bf122149f8554
test -z "$(git status --porcelain)"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export SHARDS_TENANT_STDIO_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export XDG_CACHE_HOME="$campaign/cache" TMPDIR=/tmp/mt-01a1-live-v1
set +e
timeout --signal=TERM --kill-after=30s 1800s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > "$folder/compile.jsonl" 2> "$folder/compile.log" 9>&-
compile_status="$?"
set -e
printf '%s\n' "$compile_status" > "$folder/compile.exit"
if [ "$compile_status" -ne 0 ]; then tail -80 "$folder/compile.log"; exit "$compile_status"; fi
python3 - <<'PY' > "$folder/test-binary.txt"
from pathlib import Path
import json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/caption-v4/compile.jsonl'
bins=[v['executable'] for line in p.read_text().splitlines() if (v:=json.loads(line)).get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1,bins
print(bins[0])
PY
binary="$(cat "$folder/test-binary.txt")"
. ../records/live-inputs.env
export SHARDS_TENANT_EXIT_DEADLINE_MS=300000 SHARDS_TENANT_EXIT_RECORDS="$folder/normal-records"
export SHARDS_TENANT_STDIO_CHALLENGE_HEX="$(python3 -c 'import uuid; print(uuid.uuid4().bytes.hex())')"
export SHARDS_TENANT_STDIO_TASKS=5 SHARDS_TENANT_STDIO_DEADLINE_MS=300000 SHARDS_TENANT_STDIO_WORK_MS=300000 SHARDS_TENANT_STDIO_CLEANUP_MS=300000 SHARDS_TENANT_STDIO_REPORT_BYTES=269
export SHARDS_TENANT_STDIO_RECORDS="$folder/inherited-records"
python3 - <<'PY' > "$folder/environment.json"
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith(('SHARDS_TENANT_STDIO_','SHARDS_TENANT_EXIT_')) or k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','SHARDS_KERNEL','SHARDS_TEST_KERNEL','DOCKER_CONFIG','SHARDS_REQUIRE_VMS','XDG_CACHE_HOME','TMPDIR']},indent=2))
PY
printf '%s\n' "$binary tenant_exit::member_normal_exit_preserves_sibling_and_main_progress --exact --ignored --nocapture --test-threads=1" > "$folder/normal.command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::member_normal_exit_preserves_sibling_and_main_progress --exact --ignored --nocapture --test-threads=1 > "$folder/normal.log" 2>&1 9>&-
normal_status="$?"
set -e
printf '%s\n' "$normal_status" > "$folder/normal.exit"
tail -12 "$folder/normal.log"
if [ "$normal_status" -ne 0 ]; then cat "$folder/normal.log"; exit "$normal_status"; fi
printf '%s\n' "$binary tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1" > "$folder/inherited.command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1 > "$folder/inherited.log" 2>&1 9>&-
inherited_status="$?"
set -e
printf '%s\n' "$inherited_status" > "$folder/inherited.exit"
tail -12 "$folder/inherited.log"
if [ "$inherited_status" -ne 0 ]; then cat "$folder/inherited.log"; exit "$inherited_status"; fi
python3 - <<'PY' > "$folder/result.json"
from pathlib import Path
import json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/caption-v4'
for name in ['normal','inherited']:
    assert '1 passed; 0 failed' in (p/(name+'.log')).read_text()
    record=p/(name+'-records')
    release=json.loads((record/'release.json').read_text())
    assert release['state']=='OwnerReleasedNeighborAfterGateEof' and release['gate_peer_eof']
    t=json.loads((record/'teardown.json').read_text())
    if name=='normal':assert t['owner_stop_and_observer_join']['completed'] and t['private_run_cleanup_completed'] and t['protocol_completed']
    else:assert t['owner_stop_observer_join']['complete'] and t['run_removed'] and t['protocol_complete']
print(json.dumps({'source':'0ea38a52aa534bf665516c239c4bf122149f8554','normal':0,'inherited':0,'caption_observed':True,'full_population_kernel_return':'Unknown'},indent=2))
PY
cat "$folder/result.json"
