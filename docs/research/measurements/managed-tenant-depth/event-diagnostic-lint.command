set -eu
set -o pipefail
umask 077
candidate="${SHARDS_TENANT_EVENT_REVISION:?signed diagnostic source required}"
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test -z "$(git status --porcelain)"
folder="$campaign/records/event-diagnostic-v1"
mkdir "$folder"
export SHARDS_TENANT_EVENT_RECORDS="$folder"
timeout 120 git fetch --depth 1 --no-tags origin "$candidate" > "$folder/fetch.log" 2>&1 9>&-
git switch --detach "$candidate" > "$folder/checkout.log" 2>&1
test "$(git rev-parse HEAD)" = "$candidate"
python3 - <<'PYSCOPE' > "$folder/source-readback.json"
from pathlib import Path
import subprocess,hashlib,json,os,platform
base='bbd3f22caee7d4429d1d8459e16f88b27b692bfb'
owned='crates/shards/tests/managed_output/tenant_exit.rs'
paths=subprocess.check_output(['git','diff','--name-only',base,'HEAD']).decode().splitlines()
assert all(p==owned or p.startswith('docs/research/measurements/managed-tenant-depth/') for p in paths),paths
freeze=json.loads(Path('docs/research/measurements/managed-tenant-depth/event-diagnostic-source-freeze.json').read_text())
for p,digest in freeze['files'].items():
 assert hashlib.sha256(Path(p).read_bytes()).hexdigest()==digest,p
 if p!=owned: assert Path(p).read_bytes()==subprocess.check_output(['git','show',base+':'+p]),p
assert hashlib.sha256(Path('docs/development-rules.md').read_bytes()).hexdigest()==freeze['rules_sha256']
frozen={}
for tag in ['cba6d1c32','bbd3f22ca']:
 entries=json.loads((Path('../frozen')/tag/'manifest.json').read_text())
 for e in entries:
  p=Path('../frozen')/tag/Path(e['path']).name
  assert hashlib.sha256(p.read_bytes()).hexdigest()==e['sha256'],str(p)
 frozen[tag]=entries
print(json.dumps({'source':os.environ['SHARDS_TENANT_EVENT_REVISION'],'canonical_policy':freeze['canonical_policy'],'policy_sha256':freeze['rules_sha256'],'files':freeze['files'],'changed_paths':paths,'frozen_preserved':frozen,'host':platform.platform(),'actors_started':False},indent=2))
PYSCOPE
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target" TMPDIR=/tmp/td-01a1-b57-v1
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
scripts/check-development-policy > "$folder/policy.log" 2>&1
git diff --check > "$folder/diff-check.log" 2>&1
rustfmt --check --edition 2024 --config skip_children=true crates/shards/tests/managed_output/tenant_exit.rs > "$folder/rustfmt.log" 2>&1
bash -n docs/research/measurements/managed-tenant-depth/event-diagnostic-lint.command > "$folder/command-syntax.log" 2>&1
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
print(json.dumps({'source':os.environ['SHARDS_TENANT_EVENT_REVISION'],'host_clippy_exit':int((r/'host-clippy.exit').read_text()),'log_bytes':log.stat().st_size,'log_sha256':hashlib.sha256(log.read_bytes()).hexdigest(),'post_status':subprocess.check_output(['git','status','--porcelain']).decode(),'new_vm_or_test_executions':0},indent=2))
PYRESULT
cat "$folder/result.json"
tail -30 "$folder/host-clippy.log"
exit "$code"
