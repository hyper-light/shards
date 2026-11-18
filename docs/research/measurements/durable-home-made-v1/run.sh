#!/usr/bin/env bash
# Red and green for a home made at its first durable write (#36, own-35i's commit 1b; D286
# amended, ROOT's ruling of 2026-11-18): shards-durable's tests, at the checkout (green) and
# with red-home-made.patch applied (red: a home not made takes no declaration, as before),
# in this checkout, which it leaves as it found it. Run from the repository's root, on any
# host:
#   docs/research/measurements/durable-home-made-v1/run.sh
# Output goes to green.log and red-home-made.log beside this script. Expected: green, every
# test passes; red, a_home_not_made_is_made_at_its_first_declaration_and_read fails at
# `assert_eq!(seen.len(), 1)`, the child's write declared nowhere.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
run() {
  echo "HEAD=$(git rev-parse HEAD) host=$(hostname) $(uname -srm) $(rustc --version)"
  git status --short
  cargo test -p shards-durable --profile test-release 2>&1
}
run > "$here/green.log"
green=$?
echo "green: exit $green (0 expected)"
status=$((green != 0))
if git apply "$here/red-home-made.patch"; then
  run > "$here/red-home-made.log"
  code=$?
  git apply -R "$here/red-home-made.patch"
  echo "red-home-made: exit $code (non-zero expected; its log says which assertion)"
  [ "$code" -ne 0 ] || status=1
else
  echo "red-home-made: does not apply"
  status=1
fi
exit "$status"
