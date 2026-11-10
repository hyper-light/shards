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
test "$(cat ../records/normal-v1/test.exit)" = 0
test "$(cat ../records/stdio-v1/test.exit)" = 0
folder="$campaign/records/stdio-minimum-v1"
mkdir "$folder"
binary="$(cat ../records/test-binary.txt)"
python3 - <<'PY' > "$folder/artifact-verification.json"
from pathlib import Path
import json,hashlib,os
p=Path('../frozen')/os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'][:9]
for e in json.loads((p/'manifest.json').read_text()):
    a=Path(e['path'])
    assert hashlib.sha256(a.read_bytes()).hexdigest()==e['sha256']
    assert hashlib.sha256((p/a.name).read_bytes()).hexdigest()==e['sha256']
print(json.dumps({'source':os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'],'all_seven_frozen_bytes_match':True},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT SHARDS_TENANT_STDIO_TASKS
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" SHARDS_REQUIRE_VMS=1
export XDG_CACHE_HOME="$campaign/cache" TMPDIR=/tmp/mt-01a1-current33df-v1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
python3 - <<'PY' > "$folder/inputs.env"
import socket,uuid
with socket.socket() as a,socket.socket() as b:
    a.bind(('127.0.0.1',0));b.bind(('127.0.0.1',0))
    actor=a.getsockname()[1];sibling=b.getsockname()[1]
assert actor!=sibling and actor>0 and sibling>0
print('export SHARDS_TENANT_STDIO_PORT='+str(sibling))
print('export SHARDS_TENANT_STDIO_ACTOR_PORT='+str(actor))
print('export SHARDS_TENANT_STDIO_CHALLENGE_HEX='+uuid.uuid4().bytes[:1].hex())
PY
. "$folder/inputs.env"
export SHARDS_TENANT_STDIO_DEADLINE_MS=300000 SHARDS_TENANT_STDIO_WORK_MS=300000 SHARDS_TENANT_STDIO_CLEANUP_MS=300000 SHARDS_TENANT_STDIO_REPORT_BYTES=61
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
python3 - <<'PY'
from pathlib import Path
import json,os
p=Path('../records/stdio-minimum-v1');code=int((p/'test.exit').read_text())
t=p/'records/teardown.json';c=p/'records/actor-control.json'
r={'source':os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'],'test_exit':code,'challenge_bytes':1,'report_bytes':61,'teardown':json.loads(t.read_text()) if t.exists() else None,'actor_control':json.loads(c.read_text()) if c.exists() else None,'full_population_kernel_return':'Unknown'}
(p/'result.json').write_text(json.dumps(r,indent=2)+'\n')
print(json.dumps(r,indent=2))
assert code==0 and '1 passed; 0 failed' in (p/'test.log').read_text(),r
t=r['teardown'];c=r['actor_control']
assert t['protocol_complete'] and t['run_removed'] and t['owner_stop_observer_join']['complete']
assert c['complete'] and c['sent']==c['received'] and len(c['sent'])==1+4+1
PY
