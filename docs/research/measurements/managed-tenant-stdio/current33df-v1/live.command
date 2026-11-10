set -eu
set -o pipefail
umask 077
candidate="${SHARDS_TENANT_CAMPAIGN_REVISION:?exact signed candidate required}"
campaign="$HOME/managed-tenant-current-33df-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
test "$(cat ../records/compile.exit)" = 0
test "$(cat ../records/testguest.exit)" = 0
binary="$(cat ../records/test-binary.txt)"
python3 - <<'PY' > ../records/live-artifact-verification.json
from pathlib import Path
import json,hashlib,os
p=Path('../frozen')/os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'][:9]
entries=json.loads((p/'manifest.json').read_text())
for e in entries:
    a=Path(e['path']);b=p/a.name
    assert hashlib.sha256(a.read_bytes()).hexdigest()==e['sha256']
    assert hashlib.sha256(b.read_bytes()).hexdigest()==e['sha256']
print(json.dumps({'source':os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'],'all_seven_build_and_frozen_bytes_match':True},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT SHARDS_TENANT_STDIO_TASKS
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export TMPDIR=/tmp/mt-01a1-current33df-v1
mkdir -m 700 "$TMPDIR"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
mkdir "$campaign/records/normal-v1" "$campaign/records/stdio-v1"
python3 - <<'PY' > ../records/live-inputs.env
import socket,uuid
with socket.socket() as a,socket.socket() as b:
    a.bind(('127.0.0.1',0));b.bind(('127.0.0.1',0))
    actor=a.getsockname()[1];sibling=b.getsockname()[1]
assert actor!=sibling and actor>0 and sibling>0
print('export SHARDS_TENANT_EXIT_PORT='+str(sibling))
print('export SHARDS_TENANT_STDIO_PORT='+str(sibling))
print('export SHARDS_TENANT_STDIO_ACTOR_PORT='+str(actor))
print('export SHARDS_TENANT_STDIO_CHALLENGE_HEX='+uuid.uuid4().bytes.hex())
PY
. ../records/live-inputs.env
export SHARDS_TENANT_EXIT_DEADLINE_MS=300000
export SHARDS_TENANT_EXIT_RECORDS="$campaign/records/normal-v1/records"
export SHARDS_TENANT_STDIO_DEADLINE_MS=300000 SHARDS_TENANT_STDIO_WORK_MS=300000 SHARDS_TENANT_STDIO_CLEANUP_MS=300000 SHARDS_TENANT_STDIO_REPORT_BYTES=91
export SHARDS_TENANT_STDIO_RECORDS="$campaign/records/stdio-v1/records"
python3 - <<'PY' > ../records/live-environment.json
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith(('SHARDS_TENANT_EXIT_','SHARDS_TENANT_STDIO_')) or k in ['SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','CARGO_TARGET_DIR','CARGO_BUILD_JOBS','DOCKER_CONFIG','TMPDIR','XDG_CACHE_HOME']},indent=2))
PY
printf '%s\n' "$binary tenant_exit::member_normal_exit_preserves_sibling_and_main_progress --exact --ignored --nocapture --test-threads=1" > ../records/normal-v1/command
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::member_normal_exit_preserves_sibling_and_main_progress --exact --ignored --nocapture --test-threads=1 > ../records/normal-v1/test.log 2>&1 9>&-
normal_status="$?"
set -e
printf '%s\n' "$normal_status" > ../records/normal-v1/test.exit
tail -25 ../records/normal-v1/test.log
python3 - <<'PY'
from pathlib import Path
import json,os
p=Path('../records/normal-v1');code=int((p/'test.exit').read_text())
t=p/'records/teardown.json'
receipt={'source':os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'],'test_exit':code,'teardown':json.loads(t.read_text()) if t.exists() else None,'full_population_kernel_return':'Unknown; no independent full-population/kernel witness exported'}
(p/'result.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
assert code==0 and '1 passed; 0 failed' in (p/'test.log').read_text(),receipt
assert receipt['teardown']['owner_stop_and_observer_join']['completed']
assert receipt['teardown']['protocol_completed'] and receipt['teardown']['private_run_cleanup_completed']
PY
printf '%s\n' "$binary tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime --exact --ignored --nocapture --test-threads=1" > ../records/stdio-v1/command
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime --exact --ignored --nocapture --test-threads=1 > ../records/stdio-v1/test.log 2>&1 9>&-
stdio_status="$?"
set -e
printf '%s\n' "$stdio_status" > ../records/stdio-v1/test.exit
tail -25 ../records/stdio-v1/test.log
python3 - <<'PY'
from pathlib import Path
import json,os
p=Path('../records/stdio-v1');code=int((p/'test.exit').read_text())
t=p/'records/teardown.json';c=p/'records/actor-control.json'
receipt={'source':os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'],'test_exit':code,'teardown':json.loads(t.read_text()) if t.exists() else None,'actor_control':json.loads(c.read_text()) if c.exists() else None,'full_population_kernel_return':'Unknown'}
(p/'result.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
assert code==0 and '1 passed; 0 failed' in (p/'test.log').read_text(),receipt
t=receipt['teardown'];c=receipt['actor_control']
assert t['protocol_complete'] and t['run_removed'] and t['owner_stop_observer_join']['complete']
assert c['complete'] and c['sent']==c['received']
PY
du -sk "$campaign/source" "$campaign/target" "$campaign/cache" "$campaign/records" "$campaign/frozen" > ../records/live-footprint-kib.txt
printf '%s\n' 'normal + actor stdio actual fresh cases complete'
