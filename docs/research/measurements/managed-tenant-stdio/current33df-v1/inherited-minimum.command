set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = 0ea38a52aa534bf665516c239c4bf122149f8554
test -z "$(git status --porcelain)"
folder="$campaign/records/caption-minimum-v13"
mkdir "$folder"
binary="$(cat ../records/restored-v10/test-binary.txt)"
test -x "$binary"
python3 - <<'PY' > "$folder/preflight.json"
from pathlib import Path
import json,hashlib,os
p=Path.home()/'managed-tenant-live-01a1c5d2-v1';s=os.statvfs(p)
assert s.f_bavail*s.f_frsize>=40*1024**3
caller=Path((p/'records/restored-v10/test-binary.txt').read_text().strip())
assert hashlib.sha256(caller.read_bytes()).digest()==hashlib.sha256((p/'frozen/0ea38a52a/managed_output').read_bytes()).digest()
guest=p/'source/target/guest/x86_64-unknown-linux-musl/guest/shards-testguest'
assert hashlib.sha256(guest.read_bytes()).digest()==hashlib.sha256((p/'frozen/446211e57/shards-testguest').read_bytes()).digest()
entries=json.loads((p/'records/artifacts.json').read_text())
for e in entries:
    if Path(e['path'])!=caller:
        assert hashlib.sha256(Path(e['path']).read_bytes()).hexdigest()==e['sha256'],e['path']
assert hashlib.sha256(Path('crates/testguest/src/workload.rs').read_bytes()).hexdigest()=='b548fbee772ae87a822e0603fa0588d326f0829039057235c3460944b06a2315'
assert hashlib.sha256(Path('docs/development-rules.md').read_bytes()).hexdigest()=='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3'
print(json.dumps({'source':'0ea38a52aa534bf665516c239c4bf122149f8554','source_base':'cbd39284937d024640c56db6f8d8ef2bd9b0ee24','current_policy_read_main':'33df95b08ad8f95bddeaa1c48acf0c64386e0eac','rules_sha256':'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3','caller_and_guest_frozen_bytes_match':True,'other_six_original_artifacts_match':True,'free_bytes':s.f_bavail*s.f_frsize,'ask':{'cpu':4,'memory_gib':8,'disk_gib':40},'source_changes':False,'old_records_preserved':True},indent=2))
PY
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
printf '%s  %s\n' 97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae "$SHARDS_KERNEL" | sha256sum --check > "$folder/kernel.sha256"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export SHARDS_TENANT_STDIO_SOURCE_REVISION=0ea38a52aa534bf665516c239c4bf122149f8554
export XDG_CACHE_HOME="$campaign/cache" TMPDIR=/tmp/mt-01a1-live-v1
python3 - <<'PY' > "$folder/inputs.env"
import socket,uuid
with socket.socket() as a,socket.socket() as b:
    a.bind(('127.0.0.1',0));b.bind(('127.0.0.1',0))
    actor=a.getsockname()[1];sibling=b.getsockname()[1]
assert actor!=sibling and actor>0 and sibling>0
print('export SHARDS_TENANT_EXIT_PORT='+str(sibling))
print('export SHARDS_TENANT_STDIO_PORT='+str(sibling))
print('export SHARDS_TENANT_STDIO_ACTOR_PORT='+str(actor))
print('export SHARDS_TENANT_STDIO_CHALLENGE_HEX='+uuid.uuid4().bytes[:1].hex())
PY
. "$folder/inputs.env"
export SHARDS_TENANT_EXIT_DEADLINE_MS=300000 SHARDS_TENANT_EXIT_RECORDS="$folder/normal-records"
export SHARDS_TENANT_STDIO_TASKS=5 SHARDS_TENANT_STDIO_DEADLINE_MS=300000 SHARDS_TENANT_STDIO_WORK_MS=300000 SHARDS_TENANT_STDIO_CLEANUP_MS=300000 SHARDS_TENANT_STDIO_REPORT_BYTES=239
export SHARDS_TENANT_STDIO_RECORDS="$folder/inherited-records"
python3 - <<'PY' > "$folder/environment.json"
import json,os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith(('SHARDS_TENANT_STDIO_','SHARDS_TENANT_EXIT_')) or k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','SHARDS_KERNEL','SHARDS_TEST_KERNEL','DOCKER_CONFIG','SHARDS_REQUIRE_VMS','XDG_CACHE_HOME','TMPDIR']},indent=2))
PY
printf '%s\n' "$binary tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1" > "$folder/inherited.command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1 > "$folder/inherited.log" 2>&1 9>&-
inherited_status="$?"
set -e
printf '%s\n' "$inherited_status" > "$folder/inherited.exit"
tail -14 "$folder/inherited.log"
if [ "$inherited_status" -ne 0 ]; then exit "$inherited_status"; fi
python3 - <<'PY' > "$folder/result.json"
from pathlib import Path
import json,hashlib
p=Path.home()/'managed-tenant-live-01a1c5d2-v1';f=p/'records/caption-minimum-v13'
observed={}
for name in ['inherited']:
    assert '1 passed; 0 failed' in (f/(name+'.log')).read_text()
    record=f/(name+'-records');release=json.loads((record/'release.json').read_text())
    assert release['state']=='OwnerReleasedNeighborAfterGateEof' and release['gate_peer_eof']
    t=json.loads((record/'teardown.json').read_text())
    assert t['owner_stop_observer_join']['complete'] and t['run_removed'] and t['protocol_complete']
    observed[name]={'release':release,'teardown':t}
caller=Path((p/'records/restored-v10/test-binary.txt').read_text().strip());guest=p/'source/target/guest/x86_64-unknown-linux-musl/guest/shards-testguest'
same_caller=hashlib.sha256(caller.read_bytes()).digest()==hashlib.sha256((p/'frozen/0ea38a52a/managed_output').read_bytes()).digest()
same_guest=hashlib.sha256(guest.read_bytes()).digest()==hashlib.sha256((p/'frozen/446211e57/shards-testguest').read_bytes()).digest()
print(json.dumps({'source':'0ea38a52aa534bf665516c239c4bf122149f8554','source_base':'cbd39284937d024640c56db6f8d8ef2bd9b0ee24','inherited':0,'challenge_bytes':1,'report_bytes':239,'caller_and_guest_frozen_bytes_match_after':same_caller and same_guest,'observed':observed,'full_population_kernel_return':'Unknown'},indent=2))
assert same_caller and same_guest
PY
cat "$folder/result.json"
