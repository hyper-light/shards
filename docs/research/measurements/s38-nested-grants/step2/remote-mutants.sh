# Runs on adacachy in the temporary clone: each named mutant over REV + PATCH, then the
# unmutated tree again (no mutant's build left behind), then clippy of the crate.
# Usage: bash -s REV PATCH MUTANT...
set -uo pipefail
REV=$1; PATCH=$2; shift 2
ROOT=$HOME/.cache/s38-step2; SRC=$ROOT/src
cd "$SRC" || exit 91
export RUST_TEST_THREADS=4
fresh() {
  git checkout --quiet --force "$REV" && git clean -fdq && git apply --whitespace=nowarn "$ROOT/$PATCH"
}
for m in "$@"; do
  fresh || { echo "== $m: reset failed"; exit 93; }
  python3 -I "$ROOT/mutate.py" "$m" || { echo "== $m: did not apply"; continue; }
  timeout 1500 cargo test -p shards-agentfile -j4 --no-fail-fast --test agentfile > "$ROOT/mutant-$m.log" 2>&1
  rc=$?
  echo "== mutant $m: exit=$rc; failing tests: $(grep -E '^    [a-z_0-9]+$' "$ROOT/mutant-$m.log" | sort -u | tr -d ' ' | tr '\n' ' ')"
  grep -E '^test result' "$ROOT/mutant-$m.log"
done
fresh || exit 93
timeout 1500 cargo test -p shards-agentfile -j4 --no-fail-fast > "$ROOT/restored.log" 2>&1
echo "== restored, unmutated: exit=$?"; grep -E '^test result' "$ROOT/restored.log"
timeout 1500 cargo clippy -p shards-agentfile --all-targets -j4 -- -D warnings > "$ROOT/clippy.log" 2>&1
echo "== clippy -p shards-agentfile --all-targets -D warnings: exit=$?"; grep -E '^(warning|error)' "$ROOT/clippy.log" | sort | uniq -c | head -20
timeout 300 cargo fmt -p shards-agentfile --check > "$ROOT/fmt.log" 2>&1
echo "== fmt --check -p shards-agentfile: exit=$?"; head -5 "$ROOT/fmt.log"
