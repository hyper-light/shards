timeout --signal=TERM --kill-after=15s 600s bash <<'SHARDS_DIAGNOSTIC_PREPARE'
set -eu
set -o pipefail
umask 077
candidate=249dc4cf047586bcb040ae7895bc95769cde5b57
campaign="$HOME/managed-tenant-event-diag-01a1c5d2-v2"
cd "$campaign/source"
exec 9>"$campaign/.light.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
test -r /dev/kvm
test -w /dev/kvm
python3 - <<'PYGATE'
from pathlib import Path
import json
r=Path('../records')
assert json.loads((r/'result.json').read_text())['host_clippy_exit']==0
assert (r/'build-v1/compile.exit').read_text().strip()=='0'
assert (r/'build-v1/testguest.exit').read_text().strip()=='0'
assert json.loads((r/'build-v1/artifacts.json').read_text())['all_six_product_native_bytes_match']
assert not list(r.rglob('actor-started.once'))
PYGATE
folder="$campaign/records/expiry-diagnostic-v1"
mkdir "$folder" "$folder/expiry"
export SHARDS_TENANT_DEPTH_LIVE_RECORDS="$folder"
kernel="$campaign/cache/vmlinux-6.18.48-x86_64-17c3ba205400"
if [ ! -e "$kernel" ]; then
    curl --fail --location --max-time 120 --output "$kernel" https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64 > "$folder/kernel-download.log" 2>&1
fi
python3 - <<'PYTMP'
from pathlib import Path
import os,stat
p=Path('/tmp/td-01a1-b57-v1')
if p.exists():
 s=p.lstat();assert stat.S_ISDIR(s.st_mode) and s.st_uid==os.getuid()
 assert not any(p.iterdir()),'Owned short TMPDIR is not empty; no reuse/cleanup'
else:p.mkdir(mode=0o700)
PYTMP
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_ABI_IDENTITY SHARDS_INIT
export SHARDS_TENANT_DEPTH_REVISION="$candidate"
export SHARDS_TENANT_DEPTH_BINARY="$(cat "$campaign/records/build-v1/test-binary.txt")"
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target" CARGO_HOME="$campaign/cargo-home"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache" TMPDIR=/tmp/td-01a1-b57-v1
export SHARDS_REQUIRE_VMS=1 SHARDS_KERNEL="$kernel" SHARDS_TEST_KERNEL="$kernel"
export SHARDS_TEST_GUESTS="$campaign/frozen/249dc4cf0-v1"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
export SHARDS_TENANT_STDIO_PORT=36935 SHARDS_TENANT_STDIO_ACTOR_PORT=40023
export SHARDS_TENANT_STDIO_CHALLENGE_HEX=955572fd23dc4b5a969a98aba3e7eaa3
export SHARDS_TENANT_STDIO_DEPTH=2 SHARDS_TENANT_STDIO_TASKS=6 SHARDS_TENANT_STDIO_REPORT_BYTES=269
export SHARDS_TENANT_STDIO_ACTOR_WORK_MS=120000 SHARDS_TENANT_STDIO_DEADLINE_MS=240000 SHARDS_TENANT_STDIO_WORK_MS=240000 SHARDS_TENANT_STDIO_CLEANUP_MS=15000
export SHARDS_TENANT_STDIO_RECORDS="$folder/expiry/records"
python3 - <<'PYREADBACK' > "$folder/pre-actor-readback.json"
from pathlib import Path
import json,hashlib,os,subprocess,shlex
p=Path(os.environ['SHARDS_TENANT_DEPTH_LIVE_RECORDS'])
revision=os.environ['SHARDS_TENANT_DEPTH_REVISION']
assert subprocess.check_output(['git','rev-parse','HEAD']).decode().strip()==revision
assert not list(Path('../records').rglob('actor-started.once'))
manifest=Path('../frozen/249dc4cf0-v1/manifest.json');entries=json.loads(manifest.read_text())
assert len(entries)==7
for e in entries:
 for original in [Path(e['path']),manifest.parent/Path(e['path']).name]:
  b=original.read_bytes();assert len(b)==e['bytes'] and hashlib.sha256(b).hexdigest()==e['sha256']
assert json.loads(Path('../records/build-v1/artifacts.json').read_text())['all_six_product_native_bytes_match']
static=manifest.parent/'guest/shards-testguest'
assert hashlib.sha256(static.read_bytes()).hexdigest()=='a9266f916668f0c960e69aef3e32b6acd2ce55540a3cfdc178de80e1e01d0f15'
kernel=Path(os.environ['SHARDS_KERNEL']);b=kernel.read_bytes()
assert len(b)==27758808 and hashlib.sha256(b).hexdigest()=='48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7'
freeze=json.loads(Path('docs/research/measurements/managed-tenant-depth/event-diagnostic-source-freeze.json').read_text())
for name,d in freeze['files'].items():assert hashlib.sha256(Path(name).read_bytes()).hexdigest()==d,name
names=['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','CARGO_HOME','DOCKER_CONFIG','XDG_CACHE_HOME','SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','SHARDS_TEST_GUESTS','TMPDIR']
names+=sorted(k for k in os.environ if k.startswith('SHARDS_TENANT_'))
environment={k:os.environ[k] for k in names}
expected={'SHARDS_TENANT_STDIO_PORT':'36935','SHARDS_TENANT_STDIO_ACTOR_PORT':'40023','SHARDS_TENANT_STDIO_CHALLENGE_HEX':'955572fd23dc4b5a969a98aba3e7eaa3','SHARDS_TENANT_STDIO_DEPTH':'2','SHARDS_TENANT_STDIO_TASKS':'6','SHARDS_TENANT_STDIO_REPORT_BYTES':'269','SHARDS_TENANT_STDIO_ACTOR_WORK_MS':'120000','SHARDS_TENANT_STDIO_DEADLINE_MS':'240000','SHARDS_TENANT_STDIO_WORK_MS':'240000','SHARDS_TENANT_STDIO_CLEANUP_MS':'15000'}
assert all(environment[k]==v for k,v in expected.items())
selected='tenant_exit::stdio::current_tenant_stdio::inherited_d2_guest_expiry_preserves_subsequent_neighbor_and_main'
invocation=['timeout','--signal=KILL','255s',environment['SHARDS_TENANT_DEPTH_BINARY'],selected,'--exact','--ignored','--nocapture','--test-threads=1']
(p/'exact-invocation.txt').write_text(shlex.join(invocation)+'\n')
(p/'runtime.env').write_text(''.join('export '+k+'='+shlex.quote(v)+'\n' for k,v in environment.items()))
receipt={'source':revision,'policy_at_dispatch':'ca3ab10c151aa5539e757f1cc76029746ccbb561','rules_sha256':freeze['rules_sha256'],'source_hashes':freeze['files'],'artifacts':entries,'kernel_sha256':hashlib.sha256(b).hexdigest(),'inputs':environment,'exact_invocation':invocation,'actual_new_runtime_cases':0,'original_r79_event_cause':'Unknown; old discarded Event is not recovered','independent_direct_wait_population_kernel_return':'Unknown before actors; never inferred from End/EOF'}
print(json.dumps(receipt,indent=2))
PYREADBACK
cat "$folder/pre-actor-readback.json"
SHARDS_DIAGNOSTIC_PREPARE
