set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852
test -z "$(git status --porcelain)"
folder="$campaign/records/inherited-v3-tasks5"
mkdir "$folder"
binary="$(cat ../records/inherited-v2-live/test-binary.txt)"
test -x "$binary"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_TEST_KERNEL="$SHARDS_KERNEL"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852
export SHARDS_TENANT_STDIO_SOURCE_REVISION=f4ee5f7c5ba7c263b2a8a2d84c3ff8f79d771852
export XDG_CACHE_HOME="$campaign/cache"
export TMPDIR=/tmp/mt-01a1-live-v1
. ../records/live-inputs.env
export SHARDS_TENANT_STDIO_CHALLENGE_HEX="$(python3 -c 'import uuid; print(uuid.uuid4().bytes.hex())')"
export SHARDS_TENANT_STDIO_TASKS=5
export SHARDS_TENANT_STDIO_DEADLINE_MS=300000
export SHARDS_TENANT_STDIO_WORK_MS=300000
export SHARDS_TENANT_STDIO_CLEANUP_MS=300000
export SHARDS_TENANT_STDIO_REPORT_BYTES=269
export SHARDS_TENANT_STDIO_RECORDS="$folder/records"
sha256sum "$binary" target/guest/x86_64-unknown-linux-musl/guest/shards-testguest > "$folder/binary-hashes.txt"
python3 - <<'PY' > "$folder/environment.json"
import os,json
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith('SHARDS_TENANT_STDIO_') or k in ['SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','CARGO_TARGET_DIR','CARGO_BUILD_JOBS','DOCKER_CONFIG','TMPDIR','XDG_CACHE_HOME']},indent=2))
PY
printf '%s\n' "$binary tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1" > "$folder/command"
set +e
timeout --signal=TERM --kill-after=30s 900s "$binary" tenant_exit::stdio::inherited::inherited_child_writers_survive_parent_stdio_closure --exact --ignored --nocapture --test-threads=1 > "$folder/test.log" 2>&1 9>&-
test_status="$?"
set -e
printf '%s\n' "$test_status" > "$folder/test.exit"
tail -65 "$folder/test.log"
cat "$folder/records/teardown.json"
if [ -f "$folder/records/history-before-cleanup.json" ]; then cat "$folder/records/history-before-cleanup.json"; fi
du -sk "$campaign/source" "$campaign/target" "$campaign/cache" "$campaign/frozen" "$campaign/records" > "$folder/footprint-kib.txt"
exit "$test_status"
