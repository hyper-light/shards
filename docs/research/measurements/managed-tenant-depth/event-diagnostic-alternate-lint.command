set -eu
set -o pipefail
umask 077
candidate=249dc4cf047586bcb040ae7895bc95769cde5b57
base=29189b6a8406f0f670c164abc6bffd9de19ec6ea
campaign="$HOME/managed-tenant-event-diag-01a1c5d2-v1"
mkdir "$campaign"
mkdir "$campaign/records" "$campaign/target" "$campaign/cache" "$campaign/cargo-home" "$campaign/tmp"
folder="$campaign/records"
export SHARDS_TENANT_EVENT_RECORDS="$folder" SHARDS_TENANT_EVENT_REVISION="$candidate"
exec 9>"$campaign/.light.lock"
flock -n 9
command -v cargo rustc rustfmt zig git > "$folder/tools.txt"
git init --quiet "$campaign/source"
cd "$campaign/source"
git remote add origin git@github.com:hyper-light/shards.git
timeout --signal=TERM --kill-after=15s 120s git fetch --depth 1 --no-tags origin "$candidate" "$base" > "$folder/fetch.log" 2>&1 9>&-
git switch --detach "$candidate" > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = "$candidate"
python3 - <<'PYSCOPE' > "$folder/source-readback.json"
from pathlib import Path
import subprocess,hashlib,json,os,platform,shutil
base='29189b6a8406f0f670c164abc6bffd9de19ec6ea'
owned='crates/shards/tests/managed_output/tenant_exit.rs'
paths=subprocess.check_output(['git','diff','--name-only',base,'HEAD']).decode().splitlines()
assert all(p==owned or p.startswith('docs/research/measurements/managed-tenant-depth/') for p in paths),paths
freeze=json.loads(Path('docs/research/measurements/managed-tenant-depth/event-diagnostic-source-freeze.json').read_text())
for p,digest in freeze['files'].items():
 assert hashlib.sha256(Path(p).read_bytes()).hexdigest()==digest,p
 if p!=owned: assert Path(p).read_bytes()==subprocess.check_output(['git','show',base+':'+p]),p
assert hashlib.sha256(Path('docs/development-rules.md').read_bytes()).hexdigest()==freeze['rules_sha256']
print(json.dumps({'source':os.environ['SHARDS_TENANT_EVENT_REVISION'],'canonical_policy':freeze['canonical_policy'],'policy_sha256':freeze['rules_sha256'],'files':freeze['files'],'changed_paths':paths,'host':platform.platform(),'free_bytes':shutil.disk_usage('.').free,'new_private_target':str(Path('../target').resolve()),'new_private_cargo_home':str(Path('../cargo-home').resolve()),'helper_mode':'SHARDS_HELPERS=skip, existing official check-only path; init/server from exact candidate if build script requires them','test_or_vm_executions':0},indent=2))
PYSCOPE
unset SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export SHARDS_HELPERS=skip CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target" CARGO_HOME="$campaign/cargo-home" TMPDIR="$campaign/tmp"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
scripts/check-development-policy > "$folder/policy.log" 2>&1
git diff --check > "$folder/diff-check.log" 2>&1
rustfmt --check --edition 2024 --config skip_children=true crates/shards/tests/managed_output/tenant_exit.rs > "$folder/rustfmt.log" 2>&1
rustc -Vv > "$folder/rustc.txt"
cargo -V > "$folder/cargo.txt"
set +e
timeout --signal=TERM --kill-after=15s 600s cargo clippy --locked -j4 --profile test-release -p shards --test managed_output -- -D warnings > "$folder/host-clippy.log" 2>&1 9>&-
code="$?"
set -e
printf '%s\n' "$code" > "$folder/host-clippy.exit"
python3 - <<'PYRESULT' > "$folder/result.json"
from pathlib import Path
import json,os,subprocess,hashlib
r=Path(os.environ['SHARDS_TENANT_EVENT_RECORDS'])
log=r/'host-clippy.log'
print(json.dumps({'source':os.environ['SHARDS_TENANT_EVENT_REVISION'],'host_clippy_exit':int((r/'host-clippy.exit').read_text()),'log_bytes':log.stat().st_size,'log_sha256':hashlib.sha256(log.read_bytes()).hexdigest(),'post_status':subprocess.check_output(['git','status','--porcelain']).decode(),'new_vm_or_test_executions':0,'current_runnable_artifact_gate':False},indent=2))
PYRESULT
cat "$folder/result.json"
tail -30 "$folder/host-clippy.log"
exit "$code"
