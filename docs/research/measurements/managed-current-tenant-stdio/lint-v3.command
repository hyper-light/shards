set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-current-tenant-28e-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = ${SHARDS_CURRENT_TENANT_REVISION:?exact signed source required}
test -z "$(git status --porcelain)"
folder="$campaign/records/lint-v3"
mkdir "$folder"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_TENANT_EXIT_SOURCE_REVISION=${SHARDS_CURRENT_TENANT_REVISION:?exact signed source required} SHARDS_TENANT_STDIO_SOURCE_REVISION=${SHARDS_CURRENT_TENANT_REVISION:?exact signed source required}
set +e
timeout --signal=TERM --kill-after=30s 1200s cargo clippy --locked -j4 --profile test-release -p shards --test managed_output -- -D warnings > "$folder/host.log" 2>&1 9>&-
host="$?"
set -e
printf '%s\n' "$host" > "$folder/host.exit"
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl timeout --signal=TERM --kill-after=30s 1200s cargo clippy --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" -- -D warnings > "$folder/native.log" 2>&1 9>&-
native="$?"
set -e
printf '%s\n' "$native" > "$folder/native.exit"
tail -12 "$folder/host.log"
tail -12 "$folder/native.log"
if [ "$host" -ne 0 ] || [ "$native" -ne 0 ]; then exit 1; fi
