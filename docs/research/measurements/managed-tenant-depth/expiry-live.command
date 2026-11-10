set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
candidate="${SHARDS_TENANT_DEPTH_REVISION:?exact expiry source required}"
folder="$campaign/records/expiry-v1/live"
. "$folder/runtime.env"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
test -f "$folder/pre-actor-readback.json"
test "$(cat ../records/expiry-v1/build/compile.exit)" = 0
test "$(cat ../records/expiry-v1/build/testguest.exit)" = 0
test "$(cat ../records/expiry-v1/build/host-clippy.exit)" = 0
python3 - <<'PYONCE'
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])
with (p/'actor-started.once').open('x') as f:
 json.dump({'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'maximum_actual_cases':1,'retry_or_enlargement':False},f)
PYONCE
binary="$SHARDS_TENANT_DEPTH_BINARY"
selected=tenant_exit::stdio::current_tenant_stdio::inherited_d2_guest_expiry_preserves_subsequent_neighbor_and_main
cp "$folder/exact-invocation.txt" "$folder/expiry/command"
set +e
timeout --signal=KILL 255s "$binary" "$selected" --exact --ignored --nocapture --test-threads=1 > "$folder/expiry/test.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/expiry/test.exit"
tail -24 "$folder/expiry/test.log"
python3 - <<'PYRESULT'
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])/'expiry'
t=p/'records/teardown.json';code=int((p/'test.exit').read_text());log=(p/'test.log').read_text()
result={'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'test_exit':code,'one_selected_test_passed':'1 passed; 0 failed' in log,'teardown':json.loads(t.read_text()) if t.exists() else None,'full_population_kernel_direct_wait':'Unknown','retry_or_enlargement':False}
(p/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
assert result['teardown'] is not None and result['teardown']['run_removed'] and result['teardown']['owner_stop_observer_join']['complete'],'cleanup Unknown: retain records; no retry'
assert code==0 and result['one_selected_test_passed'] and result['teardown']['protocol_complete'],result
PYRESULT
