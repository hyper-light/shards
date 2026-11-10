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
import json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/restored-v6/result.json'
r=json.loads(p.read_text())
assert r['test_exit']==0 and r['guest_binary_byte_restored'] and r['caller_binary_byte_restored'],r
PY
folder="$campaign/records/suffix-negative-v7"
mkdir "$folder"
git fetch --depth=1 origin 2a9ade8302935fb7636bf01b3e1ba7e1d094b622 > "$folder/fetch.log" 2>&1
git switch --detach 2a9ade8302935fb7636bf01b3e1ba7e1d094b622 > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = 2a9ade8302935fb7636bf01b3e1ba7e1d094b622
test -z "$(git status --porcelain)"
git show --format=fuller --no-patch HEAD > "$folder/commit.txt"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=2a9ade8302935fb7636bf01b3e1ba7e1d094b622
export SHARDS_TENANT_STDIO_SOURCE_REVISION=2a9ade8302935fb7636bf01b3e1ba7e1d094b622
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
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/suffix-negative-v7/compile.jsonl'
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
import json,struct
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/suffix-negative-v7'
code=int((p/'test.exit').read_text())
t=json.loads((p/'records/teardown.json').read_text())
control=json.loads((p/'records/actor-control.json').read_text())
header=4+struct.calcsize('>I')+struct.calcsize('>Q')
payload=bytearray()
for f in sorted((p/'records').glob('wire-1-1-*.bin'),key=lambda f:int(f.stem.rsplit('-',1)[1])):
    b=f.read_bytes()
    assert len(b)>=header and b[0]==1 and b[2]==1 and b[3]==0 and int.from_bytes(b[4:8],'big')==1
    if b[1]==1:payload.extend(b[header:])
tail=b'\0stdout\n\xff\x1b[2Jout-tail'
expected=b'agent'+tail+b'agent-inherited-ready'+tail+b'agent-inherited-after-release'+tail
mismatch=[i for i,(a,b) in enumerate(zip(payload,expected)) if a!=b]
receipt={'source':'2a9ade8302935fb7636bf01b3e1ba7e1d094b622','test_exit':code,'raw_stdout_bytes':len(payload),'mismatch_offsets':mismatch,'actor_control_complete':control['complete'],'sibling_before_child_release_record':(p/'records/sibling-before-child-release.json').exists(),'teardown':t,'proof_scope':'fixture exact-byte validity only; no ownership guard proof','full_population_kernel_return':'Unknown'}
(p/'validation-observations.json').write_text(json.dumps(receipt,indent=2)+'\n')
assert code==101 and '0 passed; 1 failed' in (p/'test.log').read_text(),receipt
assert len(mismatch)==1 and payload[mismatch[0]]==ord('T') and expected[mismatch[0]]==ord('t'),receipt
assert control['complete'] and control['sent']==control['received'] and receipt['sibling_before_child_release_record'],receipt
assert not t['protocol_complete'] and t['run_removed'] and t['owner_stop_observer_join']['complete'],receipt
print(json.dumps(receipt,indent=2))
PY
cat "$folder/result.json"
