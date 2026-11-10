set -euC
export SHARDS_HELPERS=skip
export CARGO_TARGET_DIR="$HOME/managed-cache-proof-target"
git rev-parse HEAD > "$HOME/managed-cache-fixed.revision"
sha256sum crates/shards/src/agentfile.rs > "$HOME/managed-cache-fixed.sha256"
set +e
cargo test --locked -j 4 -p shards --bin shards agentfile::tests:: -- --nocapture > "$HOME/managed-cache-green.log" 2>&1
rc=$?
set -e
printf '%s\n' "$rc" > "$HOME/managed-cache-green.exit"
test "$rc" -eq 0
git apply docs/research/measurements/managed-cache/restore-cache-mutation.patch
trap 'git restore --source=HEAD -- crates/shards/src/agentfile.rs' EXIT
set +e
cargo test --locked -j 4 -p shards --bin shards agentfile::tests::runnable_members_are_read_again_when_the_image_at_a_path_changes -- --exact --nocapture > "$HOME/managed-cache-mutation.log" 2>&1
rc=$?
set -e
printf '%s\n' "$rc" > "$HOME/managed-cache-mutation.exit"
test "$rc" -eq 101
grep -F 'active_members.is_empty()' "$HOME/managed-cache-mutation.log"
git restore --source=HEAD -- crates/shards/src/agentfile.rs
cargo test --locked -j 4 -p shards --bin shards agentfile::tests:: -- --nocapture > "$HOME/managed-cache-restored.log" 2>&1
printf '0\n' > "$HOME/managed-cache-restored.exit"
cargo clippy --locked -j 4 -p shards --bin shards -- -D warnings > "$HOME/managed-cache-clippy.log" 2>&1
printf '0\n' > "$HOME/managed-cache-clippy.exit"
tail -14 "$HOME/managed-cache-green.log"
tail -12 "$HOME/managed-cache-mutation.log"
tail -6 "$HOME/managed-cache-restored.log"
tail -3 "$HOME/managed-cache-clippy.log"
