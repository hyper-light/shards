# Runs on adacachy in the temporary clone: REV with PATCH applied, then step 2's gate.
# Usage: bash -s REV PATCH
set -uo pipefail
REV=$1; PATCH=$2
ROOT=$HOME/.cache/s38-step2; SRC=$ROOT/src
cd "$SRC" || exit 91
git cat-file -e "$REV^{commit}" 2>/dev/null || git fetch --quiet origin || { echo "fetch failed"; exit 92; }
git checkout --quiet --force "$REV" && git clean -fdq && git apply --whitespace=nowarn "$ROOT/$PATCH" || { echo "reset/apply failed"; exit 93; }
echo "== final rev=$(git rev-parse HEAD) patch=$(sha256sum "$ROOT/$PATCH" | cut -c1-16) host=$(hostname) load=$(cut -d' ' -f1-3 /proc/loadavg) $(rustc --version)"
git diff --stat | tail -1
export RUST_TEST_THREADS=4
timeout 3000 cargo test -p shards-agentfile -j4 --no-fail-fast > "$ROOT/final-test.log" 2>&1
echo "== cargo test -p shards-agentfile -j4 --no-fail-fast: exit=$?"; grep -E '^(test result|     Running)|FAILED|panicked|^warning' "$ROOT/final-test.log" | head -30
# A check links nothing and runs no guest: placeholder guests stand in for the nested
# musl builds (crates/shards/build.rs copies them into OUT_DIR, nothing more).
mkdir -p "$ROOT/guests" && : > "$ROOT/guests/shards-init" && : > "$ROOT/guests/shards-server"
SHARDS_HELPERS=skip SHARDS_INIT_BINARY="$ROOT/guests/shards-init" SHARDS_SERVER_BINARY="$ROOT/guests/shards-server" \
  timeout 3000 cargo check -p shards --tests -j4 > "$ROOT/final-check.log" 2>&1
echo "== cargo check -p shards --tests -j4 (SHARDS_HELPERS=skip): exit=$?"; grep -E '^(error|warning)(\[|:)' "$ROOT/final-check.log" | sort | uniq -c | head -20; tail -2 "$ROOT/final-check.log"
timeout 1500 cargo clippy -p shards-agentfile --all-targets -j4 -- -D warnings > "$ROOT/final-clippy.log" 2>&1
echo "== cargo clippy -p shards-agentfile --all-targets -- -D warnings: exit=$?"; grep -E '^(warning|error)' "$ROOT/final-clippy.log" | sort | uniq -c | head
timeout 600 cargo fmt --all --check > "$ROOT/final-fmt.log" 2>&1
echo "== cargo fmt --all --check: exit=$?"; head -5 "$ROOT/final-fmt.log"
python3 -I docs/research/measurements/s38-nested-grants/literal-audit.py > "$ROOT/final-audit.log" 2>&1
echo "== literal-audit.py: exit=$?"; tail -3 "$ROOT/final-audit.log"
