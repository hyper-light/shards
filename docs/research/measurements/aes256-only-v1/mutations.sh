#!/usr/bin/env bash
# Applies each named mutation of mutate.py, runs its crate's cipher tests, and restores the file
# (written anew, so its mtime is newer than the mutant's build and cargo rebuilds it). From the
# repository root, with CARGO_BUILD_JOBS and the target's environment set by the caller:
#   docs/research/measurements/aes256-only-v1/mutations.sh base-remote chacha-server ...
# A mutation's tests all passing is a surviving mutant; the red baseline is base-*.
set -u
here=docs/research/measurements/aes256-only-v1
remote=(cargo test -p shards-remote --lib -- tls::tests handshake::)
server=(cargo test -p shards-server --target x86_64-unknown-linux-gnu -- tests::an_instance_and_its_caller_are tests::a_client_limited_to)
backup=$(mktemp)
for m in "$@"; do
  file=$(python3 "$here/mutate.py" --file "$m") || exit 2
  cp "$file" "$backup"
  echo "=== $m: $file"
  python3 "$here/mutate.py" "$m" >/dev/null || exit 2
  diff -u "$backup" "$file" | tail -n +3
  case "$m" in
    *-remote) "${remote[@]}" 2>&1 | grep -v '^warning: aws-lc-sys' ;;
    *-server) "${server[@]}" 2>&1 | grep -v '^warning: aws-lc-sys' ;;
  esac
  echo "=== $m: cargo test exit ${PIPESTATUS[0]} (non-zero: the mutant is killed)"
  cat "$backup" > "$file"
  touch "$file"
  cmp -s "$backup" "$file" && echo "=== $m: $file restored"
done
rm -f "$backup"
echo "=== source after: $(git diff --stat -- crates | tail -1)"
