#!/usr/bin/env bash
# This lane's light lint set for shards-init, run from the synced worktree root on the
# Linux runner: workspace rustfmt check (scripts/lint's first step), shards-init clippy with
# scripts/lint's flags on glibc and on musl (production init is static musl), and the musl
# check the brief asks for. Each step's exit status is printed; the script exits nonzero if any failed.
set -u
status=0
step() {
  echo "== $*"
  "$@" 2>&1
  local rc=$?
  echo "== exit $rc: $*"
  [ "$rc" -eq 0 ] || status=1
}
git rev-parse HEAD
git diff --stat -- crates
step cargo fmt --all --check
step cargo clippy -p shards-init --all-targets --target x86_64-unknown-linux-gnu -j2 -- -D warnings
step cargo clippy -p shards-init --all-targets --target x86_64-unknown-linux-musl -j2 -- -D warnings
step cargo check -p shards-init --target x86_64-unknown-linux-musl -j2
exit "$status"
