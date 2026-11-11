set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-event-diag-01a1c5d2-v2"
cd "$campaign/source"
exec 9>"$campaign/.light.lock"
flock -n 9
folder="$campaign/records/expiry-diagnostic-v1"
. "$folder/runtime.env"
test "$(git rev-parse HEAD)" = 249dc4cf047586bcb040ae7895bc95769cde5b57
test -z "$(git status --porcelain)"
python3 - <<'PYONCE'
from pathlib import Path
import os,json,hashlib,subprocess
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])
assert not list(Path('../records').rglob('actor-started.once'))
readback=json.loads((p/'pre-actor-readback.json').read_text())
assert readback['source']==subprocess.check_output(['git','rev-parse','HEAD']).decode().strip()
for name,d in readback['source_hashes'].items():assert hashlib.sha256(Path(name).read_bytes()).hexdigest()==d,name
for e in readback['artifacts']:
 b=Path(e['path']).read_bytes();assert len(b)==e['bytes'] and hashlib.sha256(b).hexdigest()==e['sha256']
assert hashlib.sha256(Path(os.environ['SHARDS_KERNEL']).read_bytes()).hexdigest()==readback['kernel_sha256']
assert json.loads(Path('../records/build-v1/artifacts.json').read_text())['all_six_product_native_bytes_match']
assert json.loads(Path('../records/result.json').read_text())['host_clippy_exit']==0
with (p/'actor-started.once').open('x') as f:json.dump({'source':readback['source'],'maximum_actual_cases':1,'retry_or_enlargement':False,'pre_actor_readback_sha256':hashlib.sha256((p/'pre-actor-readback.json').read_bytes()).hexdigest()},f)
PYONCE
cp "$folder/exact-invocation.txt" "$folder/expiry/command"
selected=tenant_exit::stdio::current_tenant_stdio::inherited_d2_guest_expiry_preserves_subsequent_neighbor_and_main
set +e
timeout --signal=KILL 255s "$SHARDS_TENANT_DEPTH_BINARY" "$selected" --exact --ignored --nocapture --test-threads=1 > "$folder/expiry/test.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/expiry/test.exit"
python3 - <<'PYRESULT'
from pathlib import Path
import json,os
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])/'expiry'
code=int((p/'test.exit').read_text());log=(p/'test.log').read_text();t=p/'records/teardown.json'
diagnostics=[]
for d in sorted((p/'records').glob('non-record-*.json')):
 diagnostics.append({'path':str(d),'event':json.loads(d.read_text())})
result={'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'test_exit':code,'one_selected_test_passed':'1 passed; 0 failed; 0 ignored' in log,'teardown':json.loads(t.read_text()) if t.exists() else None,'private_non_record_diagnostics':diagnostics,'independent_direct_wait_population_parent_kernel':'Unknown','retry_or_enlargement':False,'original_r79_discarded_event':'Unknown; fresh event is a separate observation'}
(p/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({k:v for k,v in result.items() if k!='private_non_record_diagnostics'},indent=2))
assert result['teardown'] is not None and result['teardown']['run_removed'] and result['teardown']['owner_stop_observer_join']['complete'],'Cleanup Unknown; preserve partial raw and do not retry'
assert code==0 and result['one_selected_test_passed'] and result['teardown']['protocol_complete'],'Strict runtime outcome failed; preserve exact private diagnostic and do not retry'
PYRESULT
