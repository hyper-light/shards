set -eu
set -o pipefail
umask 077
candidate="${SHARDS_TENANT_DEPTH_REVISION:?exact signed composed candidate required}"
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
build="$campaign/records"
test "$(cat "$build/compile.exit")" = 0
test "$(cat "$build/testguest.exit")" = 0
binary="$campaign/frozen/${candidate:0:9}/$(basename "$(cat "$build/test-binary.txt")")"
export SHARDS_TENANT_DEPTH_BINARY="$binary"
folder="$campaign/records/live-single-v1"
mkdir "$folder" "$folder/normal" "$folder/single"
export SHARDS_TENANT_DEPTH_LIVE_RECORDS="$folder"
python3 - <<'PY' > "$folder/artifact-verification.json"
from pathlib import Path
import os,json,hashlib
p=Path('../frozen')/os.environ['SHARDS_TENANT_DEPTH_REVISION'][:9]
for e in json.loads((p/'manifest.json').read_text()):
    a=Path(e['path']);assert hashlib.sha256(a.read_bytes()).hexdigest()==e['sha256']
    assert hashlib.sha256((p/a.name).read_bytes()).hexdigest()==e['sha256']
print(json.dumps({'source':os.environ['SHARDS_TENANT_DEPTH_REVISION'],'all_seven_frozen_bytes_match':True},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$campaign/cache/vmlinux-6.18.48-x86_64-17c3ba205400"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
export TMPDIR=/tmp/td-01a1-b57-v1
test -d "$TMPDIR"
test -f "$campaign/frozen/${candidate:0:9}/guest/shards-testguest"
export SHARDS_TEST_GUESTS="$campaign/frozen/${candidate:0:9}"
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
unset SHARDS_TENANT_STDIO_DEPTH
export SHARDS_TENANT_STDIO_TASKS=5 SHARDS_TENANT_STDIO_DEADLINE_MS=240000 SHARDS_TENANT_STDIO_WORK_MS=240000 SHARDS_TENANT_STDIO_CLEANUP_MS=15000 SHARDS_TENANT_STDIO_REPORT_BYTES=269 SHARDS_TENANT_STDIO_RECORDS="$folder/single/records"
python3 - <<'PY' > "$folder/environment.json"
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith(('SHARDS_TENANT_','SHARDS_CURRENT_')) or k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','TMPDIR','DOCKER_CONFIG','XDG_CACHE_HOME']},indent=2))
PY
python3 - <<'PYREADBACK' > "$folder/pre-actor-readback.json"
from pathlib import Path
import hashlib,json,os,subprocess,shlex
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])
revision=os.environ['SHARDS_TENANT_DEPTH_REVISION']
assert subprocess.check_output(['git','rev-parse','HEAD']).decode().strip()==revision
manifest=Path('../frozen')/revision[:9]/'manifest.json'
entries=json.loads(manifest.read_text())
assert len(entries)==7
for e in entries:
    assert hashlib.sha256(Path(e['path']).read_bytes()).hexdigest()==e['sha256']
    assert hashlib.sha256((manifest.parent/Path(e['path']).name).read_bytes()).hexdigest()==e['sha256']
static=manifest.parent/'guest/shards-testguest'
assert hashlib.sha256(static.read_bytes()).hexdigest()==next(e['sha256'] for e in entries if Path(e['path']).name=='shards-testguest')
kernel=Path(os.environ['SHARDS_KERNEL'])
assert hashlib.sha256(kernel.read_bytes()).hexdigest()=='48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7'
freeze=json.loads(Path('docs/research/measurements/managed-tenant-depth/source-freeze-v1.json').read_text())
for name,digest in freeze['files'].items(): assert hashlib.sha256(Path(name).read_bytes()).hexdigest()==digest
names=['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','DOCKER_CONFIG','XDG_CACHE_HOME','SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','SHARDS_TEST_GUESTS','TMPDIR']
names+=sorted(k for k in os.environ if k.startswith('SHARDS_TENANT_'))
environment={k:os.environ[k] for k in names}
(p/'runtime.env').write_text(''.join('export '+k+'='+shlex.quote(v)+'\n' for k,v in environment.items()))
print(json.dumps({'source':revision,'source_hashes':freeze['files'],'artifacts':entries,'kernel_sha256':hashlib.sha256(kernel.read_bytes()).hexdigest(),'inputs':environment,'native_child_waitstatus_full_population_kernel_return':'Unknown before actors; not inferred from EOF'},indent=2))
PYREADBACK
cat "$folder/pre-actor-readback.json"
