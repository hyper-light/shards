set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test -z "$(git status --porcelain)"
mkdir "$campaign/frozen/10091363c"
cp "$(cat ../records/close-negative-v5/test-binary.txt)" "$campaign/frozen/10091363c/managed_output"
cp target/guest/x86_64-unknown-linux-musl/guest/shards-testguest "$campaign/frozen/10091363c/shards-testguest"
folder="$campaign/records/restored-v6"
mkdir "$folder"
git fetch --depth=1 origin 0ea38a52aa534bf665516c239c4bf122149f8554 > "$folder/fetch.log" 2>&1
git switch --detach 0ea38a52aa534bf665516c239c4bf122149f8554 > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = 0ea38a52aa534bf665516c239c4bf122149f8554
test -z "$(git status --porcelain)"
git show --format=fuller --no-patch HEAD > "$folder/commit.txt"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export SHARDS_TENANT_STDIO_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export XDG_CACHE_HOME="$campaign/cache" TMPDIR=/tmp/mt-01a1-live-v1
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl timeout --signal=TERM --kill-after=30s 1200s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" > "$folder/testguest.log" 2>&1 9>&-
guest_status="$?"
set -e
printf '%s\n' "$guest_status" > "$folder/testguest.exit"
if [ "$guest_status" -ne 0 ]; then tail -80 "$folder/testguest.log"; exit "$guest_status"; fi
set +e
timeout --signal=TERM --kill-after=30s 1200s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > "$folder/compile.jsonl" 2> "$folder/compile.log" 9>&-
compile_status="$?"
set -e
printf '%s\n' "$compile_status" > "$folder/compile.exit"
if [ "$compile_status" -ne 0 ]; then tail -80 "$folder/compile.log"; exit "$compile_status"; fi
python3 - <<'PY' > "$folder/test-binary.txt"
from pathlib import Path
import json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/restored-v6/compile.jsonl'
bins=[v['executable'] for line in p.read_text().splitlines() if (v:=json.loads(line)).get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1,bins
print(bins[0])
PY
binary="$(cat "$folder/test-binary.txt")"
. ../records/live-inputs.env
export SHARDS_TENANT_STDIO_CHALLENGE_HEX="$(python3 -c 'import uuid; print(uuid.uuid4().bytes.hex())')"
export SHARDS_TENANT_STDIO_TASKS=5 SHARDS_TENANT_STDIO_DEADLINE_MS=300000 SHARDS_TENANT_STDIO_WORK_MS=300000 SHARDS_TENANT_STDIO_CLEANUP_MS=300000 SHARDS_TENANT_STDIO_REPORT_BYTES=269
export SHARDS_TENANT_STDIO_RECORDS="$folder/records"
python3 - <<'PY' > "$folder/environment.json"
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith('SHARDS_TENANT_STDIO_') or k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','SHARDS_KERNEL','SHARDS_TEST_KERNEL','DOCKER_CONFIG','SHARDS_REQUIRE_VMS','XDG_CACHE_HOME','TMPDIR']},indent=2))
PY
sha256sum "$binary" target/guest/x86_64-unknown-linux-musl/guest/shards-testguest > "$folder/binary-hashes.txt"
printf '%s\n' "$binary tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1" > "$folder/command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1 > "$folder/test.log" 2>&1 9>&-
test_status="$?"
set -e
printf '%s\n' "$test_status" > "$folder/test.exit"
tail -24 "$folder/test.log"
python3 - <<'PY' > "$folder/result.json"
from pathlib import Path
import json,hashlib
root=Path.home()/'managed-tenant-live-01a1c5d2-v1'
p=root/'records/restored-v6'
code=int((p/'test.exit').read_text())
t=json.loads((p/'records/teardown.json').read_text())
binary=Path((p/'test-binary.txt').read_text().strip())
guest=root/'source/target/guest/x86_64-unknown-linux-musl/guest/shards-testguest'
same_guest=hashlib.sha256(guest.read_bytes()).digest()==hashlib.sha256((root/'frozen/446211e57/shards-testguest').read_bytes()).digest()
same_caller=hashlib.sha256(binary.read_bytes()).digest()==hashlib.sha256((root/'frozen/0ea38a52a/managed_output').read_bytes()).digest()
receipt={'source':'0ea38a52aa534bf665516c239c4bf122149f8554','test_exit':code,'guest_binary_byte_restored':same_guest,'caller_binary_byte_restored':same_caller,'teardown':t,'full_population_kernel_return':'Unknown'}
(p/'validation-observations.json').write_text(json.dumps(receipt,indent=2)+'\n')
assert code==0 and '1 passed; 0 failed' in (p/'test.log').read_text(),receipt
assert same_guest and same_caller,receipt
assert t['protocol_complete'] and t['run_removed'] and t['owner_stop_observer_join']['complete'],receipt
print(json.dumps(receipt,indent=2))
PY
cat "$folder/result.json"
