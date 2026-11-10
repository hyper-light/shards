set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
candidate="${SHARDS_TENANT_DEPTH_REVISION:?exact candidate required}"
folder="$campaign/records/live-v1"
. "$folder/runtime.env"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
test -f "$folder/pre-actor-readback.json"
test "$(cat ../records/lint-v1/host.exit)" = 0
test "$(cat ../records/lint-v1/native.exit)" = 0
binary="$SHARDS_TENANT_DEPTH_BINARY"
for case in normal depth; do
  if [ "$case" = normal ]; then
    selected=tenant_exit::stdio::current_tenant_stdio::normal_exit_preserves_current_neighbor_and_main
  else
    selected=tenant_exit::stdio::current_tenant_stdio::inherited_descendant_depth_preserves_neighbor_and_main
  fi
  printf '%s\n' "timeout --signal=KILL 255s $binary $selected --exact --ignored --nocapture --test-threads=1" > "$folder/$case/command"
  set +e
  timeout --signal=KILL 255s "$binary" "$selected" --exact --ignored --nocapture --test-threads=1 > "$folder/$case/test.log" 2>&1 9>&-
  code="$?"
  set -e
  printf '%s\n' "$code" > "$folder/$case/test.exit"
  tail -14 "$folder/$case/test.log"
  SHARDS_TENANT_DEPTH_CASE="$case" python3 - <<'PY'
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])/os.environ['SHARDS_TENANT_DEPTH_CASE']
t=p/'records/teardown.json';code=int((p/'test.exit').read_text())
r={'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'test_exit':code,'one_selected_test_passed':'1 passed; 0 failed' in (p/'test.log').read_text(),'teardown':json.loads(t.read_text()) if t.exists() else None,'full_population_kernel_parent_return':'Unknown'}
(p/'result.json').write_text(json.dumps(r,indent=2)+'\n');print(json.dumps(r))
t=r['teardown']
assert t is not None and t['run_removed'] and t['owner_stop_observer_join']['complete'],'cleanup unknown; retain before next case'
PY
done
python3 - <<'PY' > "$folder/result.json"
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS']);rows={n:json.loads((p/n/'result.json').read_text()) for n in ['normal','depth']}
print(json.dumps({'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'cases':rows,'full_population_kernel_parent_return':'Unknown'},indent=2))
assert all(v['test_exit']==0 and v['one_selected_test_passed'] and v['teardown']['protocol_complete'] for v in rows.values()),rows
PY
cat "$folder/result.json"

python3 - <<'PYCAPTION'
from pathlib import Path
import os,json
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])
for case in ['normal','depth']:
    v=json.loads((p/case/'records/release.json').read_text())
    assert v['state']=='OwnerReleasedNeighborAfterGateEof' and v['gate_peer_eof']
print('both shared release captions match actual gate EOF; terminal assertions remain in selected tests')
PYCAPTION
