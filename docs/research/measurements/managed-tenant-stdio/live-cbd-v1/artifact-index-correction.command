set -eu
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
test "$(cat ../records/compile.exit)" = 0
test "$(cat ../records/testguest.exit)" = 0
mv ../records/artifacts.json ../records/artifacts.initial-invalid.json
python3 - <<'PY' > ../records/artifacts.json
from pathlib import Path
import json,hashlib
record=Path('../records')
packets=[json.loads(line) for line in (record/'compile.jsonl').read_text().splitlines()]
binaries=[v['executable'] for v in packets if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(binaries)==1,binaries
dirs=[Path(v['out_dir']) for v in packets if v.get('reason')=='build-script-executed' and any(k=='SHARDS_HELPERS_CARRIED' for k,_ in v.get('env',[]))]
assert len(dirs)==1,dirs
out=dirs[0]
paths=[Path(binaries[0]),Path('../target/test-release/shards'),Path('target/guest/x86_64-unknown-linux-musl/guest/shards-testguest')]
paths.extend(out/n for n in ['shards-init','shards-server','helpers/shards-vm','helpers/shards-net'])
entries=[{'path':str(p.resolve()),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in paths]
assert len(entries)==7 and all(v['bytes']>0 for v in entries),entries
print(json.dumps(entries,indent=2))
(record/'artifact-index-correction.json').write_text(json.dumps({'initial_run':'r43','host_compile_exit':0,'testguest_compile_exit':0,'initial_index_exit':1,'cause':'index used wrong Cargo package-id predicate and carried/ instead of actual helpers/ path','fix':'read SHARDS_HELPERS_CARRIED build-script packet and actual helpers paths','source_changed':False,'rebuilt':False},indent=2)+'\n')
PY
du -sk "$campaign/source" "$campaign/target" "$campaign/records" > ../records/build-footprint-kib.txt
cat ../records/artifacts.json ../records/build-footprint-kib.txt
