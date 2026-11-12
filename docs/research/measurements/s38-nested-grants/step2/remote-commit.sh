# Runs on adacachy in the temporary clone: the exact commit REV, fetched from origin, then
# step 2's gate. Usage: bash -s REV BRANCH
set -uo pipefail
REV=$1; BRANCH=$2
ROOT=$HOME/.cache/s38-step2; SRC=$ROOT/src
cd "$SRC" || exit 91
git fetch --quiet origin "$BRANCH" main || { echo "fetch failed"; exit 92; }
git checkout --quiet --force "$REV" && git clean -fdq || { echo "checkout failed"; exit 93; }
echo "== commit rev=$(git rev-parse HEAD) status=[$(git status --short | wc -l) changes] host=$(hostname) load=$(cut -d' ' -f1-3 /proc/loadavg) $(rustc --version)"
export RUST_TEST_THREADS=4
timeout 3000 cargo test -p shards-agentfile -j4 --no-fail-fast > "$ROOT/commit-test.log" 2>&1
echo "== cargo test -p shards-agentfile -j4 --no-fail-fast: exit=$?"; grep -E '^(test result|     Running)|FAILED|panicked|^warning' "$ROOT/commit-test.log" | head -30
mkdir -p "$ROOT/guests" && : > "$ROOT/guests/shards-init" && : > "$ROOT/guests/shards-server"
SHARDS_HELPERS=skip SHARDS_INIT_BINARY="$ROOT/guests/shards-init" SHARDS_SERVER_BINARY="$ROOT/guests/shards-server" \
  timeout 3000 cargo check -p shards --tests -j4 > "$ROOT/commit-check.log" 2>&1
echo "== cargo check -p shards --tests -j4 (SHARDS_HELPERS=skip): exit=$?"; grep -E '^(error|warning)(\[|:)' "$ROOT/commit-check.log" | grep -v 'aws-lc-sys' | sort | uniq -c | head; tail -1 "$ROOT/commit-check.log"
timeout 1500 cargo clippy -p shards-agentfile --all-targets -j4 -- -D warnings > "$ROOT/commit-clippy.log" 2>&1
echo "== cargo clippy -p shards-agentfile --all-targets -- -D warnings: exit=$?"; grep -E '^(warning|error)' "$ROOT/commit-clippy.log" | sort | uniq -c | head
timeout 600 cargo fmt --all --check > "$ROOT/commit-fmt.log" 2>&1
echo "== cargo fmt --all --check: exit=$?"; head -5 "$ROOT/commit-fmt.log"
python3 -I docs/research/measurements/s38-nested-grants/literal-audit.py > "$ROOT/commit-audit.log" 2>&1
echo "== literal-audit.py: exit=$?"; tail -1 "$ROOT/commit-audit.log"
