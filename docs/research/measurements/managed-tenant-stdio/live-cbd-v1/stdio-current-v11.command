set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = 0ea38a52aa534bf665516c239c4bf122149f8554
test -z "$(git status --porcelain)"
folder="$campaign/records/stdio-current-v11"
mkdir "$folder"
binary="$(cat ../records/restored-v10/test-binary.txt)"
python3 - <<'PY' > "$folder/baseline-byte-comparison.json"
from pathlib import Path
import hashlib,json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1'
a=Path((p/'records/restored-v10/test-binary.txt').read_text().strip())
b=p/'source/target/guest/x86_64-unknown-linux-musl/guest/shards-testguest'
same_caller=hashlib.sha256(a.read_bytes()).digest()==hashlib.sha256((p/'frozen/0ea38a52a/managed_output').read_bytes()).digest()
same_guest=hashlib.sha256(b.read_bytes()).digest()==hashlib.sha256((p/'frozen/446211e57/shards-testguest').read_bytes()).digest()
assert same_caller and same_guest
print(json.dumps({'source':'0ea38a52aa534bf665516c239c4bf122149f8554','caller_byte_identical':same_caller,'guest_byte_identical':same_guest},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT SHARDS_TENANT_STDIO_TASKS
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_STDIO_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export XDG_CACHE_HOME="$campaign/cache" TMPDIR=/tmp/mt-01a1-live-v1
. ../records/live-inputs.env
export SHARDS_TENANT_STDIO_CHALLENGE_HEX="$(python3 -c 'import uuid; print(uuid.uuid4().bytes.hex())')"
export SHARDS_TENANT_STDIO_DEADLINE_MS=300000 SHARDS_TENANT_STDIO_WORK_MS=300000 SHARDS_TENANT_STDIO_CLEANUP_MS=300000 SHARDS_TENANT_STDIO_REPORT_BYTES=91
export SHARDS_TENANT_STDIO_RECORDS="$folder/records"
python3 - <<'PY' > "$folder/environment.json"
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith('SHARDS_TENANT_STDIO_') or k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','SHARDS_KERNEL','SHARDS_TEST_KERNEL','DOCKER_CONFIG','SHARDS_REQUIRE_VMS','XDG_CACHE_HOME','TMPDIR']},indent=2))
PY
printf '%s\n' "$binary tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime --exact --ignored --nocapture --test-threads=1" > "$folder/command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime --exact --ignored --nocapture --test-threads=1 > "$folder/test.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/test.exit"
tail -25 "$folder/test.log"
python3 - <<'PY' > "$folder/result.json"
from pathlib import Path
import json
p=Path.home()/'managed-tenant-live-01a1c5d2-v1/records/stdio-current-v11'
code=int((p/'test.exit').read_text())
t=json.loads((p/'records/teardown.json').read_text())
control=json.loads((p/'records/actor-control.json').read_text())
receipt={'source':'0ea38a52aa534bf665516c239c4bf122149f8554','test_exit':code,'actor_control':control,'teardown':t,'full_population_kernel_return':'Unknown'}
(p/'validation-observations.json').write_text(json.dumps(receipt,indent=2)+'\n')
assert code==0 and '1 passed; 0 failed' in (p/'test.log').read_text(),receipt
assert control['complete'] and control['sent']==control['received'],receipt
assert t['protocol_complete'] and t['run_removed'] and t['owner_stop_observer_join']['complete'],receipt
print(json.dumps(receipt,indent=2))
PY
cat "$folder/result.json"
