set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "446211e57d302729832ece92e55cfa13713b7d7a"
test -z "$(git status --porcelain)"
test "$(cat ../records/compile.exit)" = 0
test "$(cat ../records/testguest.exit)" = 0
binary="$(cat ../records/test-binary.txt)"
test -x "$binary"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=446211e57d302729832ece92e55cfa13713b7d7a
export SHARDS_TENANT_STDIO_SOURCE_REVISION=446211e57d302729832ece92e55cfa13713b7d7a
export XDG_CACHE_HOME="$campaign/cache"
mkdir "$campaign/cache"
export TMPDIR=/tmp/mt-01a1-live-v1
mkdir -m 700 "$TMPDIR"
mkdir "$campaign/records/normal-v1" "$campaign/records/inherited-v1"
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
export SHARDS_TENANT_STDIO_TASKS=4
export SHARDS_TENANT_STDIO_DEADLINE_MS=300000
export SHARDS_TENANT_STDIO_WORK_MS=300000
export SHARDS_TENANT_STDIO_CLEANUP_MS=300000
export SHARDS_TENANT_STDIO_REPORT_BYTES=269
export SHARDS_TENANT_STDIO_RECORDS="$campaign/records/inherited-v1/records"
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
cat ../records/normal-v1/test.log
python3 - <<'PY'
from pathlib import Path
import json
folder=Path('../records/normal-v1')
code=int((folder/'test.exit').read_text())
t=folder/'records/teardown.json'
receipt={'exit':code,'teardown':json.loads(t.read_text()) if t.exists() else None,'full_population_kernel_return':'Unknown; no independent full-population/kernel witness exported'}
(folder/'result.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
if code!=0:raise SystemExit(code)
assert '1 passed; 0 failed' in (folder/'test.log').read_text()
assert receipt['teardown']['owner_stop_and_observer_join']['completed']
assert receipt['teardown']['private_run_cleanup_completed']
PY
printf '%s\n' "$binary tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1" > ../records/inherited-v1/command
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1 > ../records/inherited-v1/test.log 2>&1 9>&-
inherited_status="$?"
set -e
printf '%s\n' "$inherited_status" > ../records/inherited-v1/test.exit
cat ../records/inherited-v1/test.log
python3 - <<'PY'
from pathlib import Path
import json
folder=Path('../records/inherited-v1')
code=int((folder/'test.exit').read_text())
t=folder/'records/teardown.json'
receipt={'exit':code,'teardown':json.loads(t.read_text()) if t.exists() else None,'full_population_kernel_return':'Unknown; no independent full-population/kernel witness exported'}
(folder/'result.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt,indent=2))
if code!=0:raise SystemExit(code)
assert '1 passed; 0 failed' in (folder/'test.log').read_text()
assert receipt['teardown']['protocol_complete']
assert receipt['teardown']['run_removed']
assert receipt['teardown']['owner_stop_observer_join']['complete']
PY
du -sk "$campaign/source" "$campaign/target" "$campaign/cache" "$campaign/records" > ../records/live-footprint-kib.txt
printf '%s\n' 'normal + inherited actual fresh cases complete'
