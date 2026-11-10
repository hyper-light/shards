set -eu
set -o pipefail
umask 077
candidate="${SHARDS_CURRENT_TENANT_REVISION:?exact signed composed candidate required}"
campaign="$HOME/managed-current-tenant-28e-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
build="$campaign/records/candidate-v2"
test "$(cat "$build/compile.exit")" = 0
test "$(cat "$build/testguest.exit")" = 0
binary="$(cat "$build/test-binary.txt")"
folder="$campaign/records/live-v2"
mkdir "$folder" "$folder/normal" "$folder/inherited"
export SHARDS_CURRENT_LIVE_RECORDS="$folder"
python3 - <<'PY' > "$folder/artifact-verification.json"
from pathlib import Path
import os,json,hashlib
p=Path('../frozen')/os.environ['SHARDS_CURRENT_TENANT_REVISION'][:9]
for e in json.loads((p/'manifest.json').read_text()):
    a=Path(e['path']);assert hashlib.sha256(a.read_bytes()).hexdigest()==e['sha256']
    assert hashlib.sha256((p/a.name).read_bytes()).hexdigest()==e['sha256']
print(json.dumps({'source':os.environ['SHARDS_CURRENT_TENANT_REVISION'],'all_seven_frozen_bytes_match':True},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
export TMPDIR=/tmp/ct-01a1-28e-v1
test -d "$TMPDIR"
python3 - <<'PY' > "$folder/inputs.env"
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
. "$folder/inputs.env"
export SHARDS_TENANT_EXIT_DEADLINE_MS=240000 SHARDS_TENANT_EXIT_RECORDS="$folder/normal/records"
export SHARDS_TENANT_STDIO_TASKS=5 SHARDS_TENANT_STDIO_DEADLINE_MS=240000 SHARDS_TENANT_STDIO_WORK_MS=240000 SHARDS_TENANT_STDIO_CLEANUP_MS=15000 SHARDS_TENANT_STDIO_REPORT_BYTES=269 SHARDS_TENANT_STDIO_RECORDS="$folder/inherited/records"
python3 - <<'PY' > "$folder/environment.json"
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith(('SHARDS_TENANT_','SHARDS_CURRENT_')) or k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','TMPDIR','DOCKER_CONFIG','XDG_CACHE_HOME']},indent=2))
PY
for case in normal inherited; do
  if [ "$case" = normal ]; then
    selected=tenant_exit::stdio::current_tenant_stdio::normal_exit_preserves_current_neighbor_and_main
  else
    selected=tenant_exit::stdio::current_tenant_stdio::inherited_child_writers_survive_parent_stdio_closure
  fi
  printf '%s\n' "timeout --signal=KILL 255s $binary $selected --exact --ignored --nocapture --test-threads=1" > "$folder/$case/command"
  set +e
  timeout --signal=KILL 255s "$binary" "$selected" --exact --ignored --nocapture --test-threads=1 > "$folder/$case/test.log" 2>&1 9>&-
  code="$?"
  set -e
  printf '%s\n' "$code" > "$folder/$case/test.exit"
  tail -14 "$folder/$case/test.log"
  SHARDS_CURRENT_CASE="$case" python3 - <<'PY'
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_CURRENT_LIVE_RECORDS'])/os.environ['SHARDS_CURRENT_CASE']
t=p/'records/teardown.json';code=int((p/'test.exit').read_text())
r={'source':os.environ['SHARDS_CURRENT_TENANT_REVISION'],'test_exit':code,'one_selected_test_passed':'1 passed; 0 failed' in (p/'test.log').read_text(),'teardown':json.loads(t.read_text()) if t.exists() else None,'full_population_kernel_parent_return':'Unknown'}
(p/'result.json').write_text(json.dumps(r,indent=2)+'\n');print(json.dumps(r))
t=r['teardown']
assert t is not None and t['run_removed'] and t['owner_stop_observer_join']['complete'],'cleanup unknown; retain before next case'
PY
done
python3 - <<'PY' > "$folder/result.json"
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_CURRENT_LIVE_RECORDS']);rows={n:json.loads((p/n/'result.json').read_text()) for n in ['normal','inherited']}
print(json.dumps({'source':os.environ['SHARDS_CURRENT_TENANT_REVISION'],'cases':rows,'full_population_kernel_parent_return':'Unknown'},indent=2))
assert all(v['test_exit']==0 and v['one_selected_test_passed'] and v['teardown']['protocol_complete'] for v in rows.values()),rows
PY
cat "$folder/result.json"
