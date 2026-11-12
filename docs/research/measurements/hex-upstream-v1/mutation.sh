#!/usr/bin/env bash
# Runs cargo test with a guard-removal mutation applied, then reverts it (R10):
#   docs/research/measurements/hex-upstream-v1/mutation.sh LABEL PATCH -- CARGO_TEST_ARGS...
# Prints the run's exit status and its test results; the whole log goes to
# $HOME/g121-tmp/hex-upstream-v1/LABEL/mutation.log. A mutation whose test still passes, or
# whose build fails, says so.
set -u
label=${1:?label}
patch=${2:?patch}
shift 2
[ "${1:-}" = -- ] && shift
if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench is running: not starting" >&2; exit 75; fi
export TMPDIR=$HOME/g121-tmp
export PATH=$HOME/.local/share/shards-zig/zig-x86_64-linux-0.16.0:$PATH
export SHARDS_HELPERS=skip
out=$HOME/g121-tmp/hex-upstream-v1/$label
mkdir -p "$out"
git apply --check "$patch" || { echo "the mutation does not apply"; exit 2; }
git apply "$patch"
trap 'git apply -R "$patch"; git diff --stat -- $(git apply --numstat "$patch" | cut -f3) | tail -1' EXIT
{ echo "\$ cargo test $*"; git rev-parse HEAD; cat "$patch"; } > "$out/mutation.log"
systemd-run --user --scope -q -p MemoryMax=24G cargo test "$@" >> "$out/mutation.log" 2>&1
status=$?
echo "mutation $(basename "$patch"): cargo test exit $status"
grep -E '^test .* (FAILED|ok)$|^test result|^error(\[|:)|panicked at' "$out/mutation.log" | head -20
