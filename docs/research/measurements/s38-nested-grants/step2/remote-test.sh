# Runs on adacachy: the temporary clone at REV, with PATCH (a file under ~/.cache/s38-step2)
# applied when given, then optionally MUTATION (a shell command run in the clone), then
# `cargo test -p shards-agentfile` within the brief's envelope (-j4, 4 test threads).
# Usage: bash -s REV LABEL [PATCH|-] [MUTATION]
set -uo pipefail
REV=$1; LABEL=$2; PATCH=${3:--}; MUT=${4:-}
ROOT=$HOME/.cache/s38-step2; SRC=$ROOT/src
mkdir -p "$ROOT"
if [ ! -d "$SRC/.git" ]; then
  git clone --quiet --filter=blob:none --no-checkout git@github.com:hyper-light/shards.git "$SRC" || { echo "clone failed"; exit 90; }
fi
cd "$SRC" || exit 91
git cat-file -e "$REV^{commit}" 2>/dev/null || git fetch --quiet origin || { echo "fetch failed"; exit 92; }
git checkout --quiet --force "$REV" || { echo "checkout failed"; exit 93; }
git clean -fdq
if [ "$PATCH" != "-" ]; then git apply --whitespace=nowarn "$ROOT/$PATCH" || { echo "patch failed"; exit 95; }; fi
echo "== $LABEL rev=$(git rev-parse HEAD) patch=$PATCH sha=$( [ "$PATCH" != "-" ] && sha256sum "$ROOT/$PATCH" | cut -c1-16) host=$(hostname) load=$(cut -d' ' -f1-3 /proc/loadavg)"
if [ -n "$MUT" ]; then echo "== mutation: $MUT"; bash -c "$MUT" || { echo "mutation failed to apply"; exit 94; }; git diff --stat -- crates/agentfile/src; fi
echo "toolchain: $(rustc --version)"
export RUST_TEST_THREADS=4
timeout 3000 cargo test -p shards-agentfile -j4 --no-fail-fast > "$ROOT/$LABEL.log" 2>&1
rc=$?
grep -E '^(test result|     Running|error(\[|:)|warning|failures:|---- )|FAILED|panicked' "$ROOT/$LABEL.log" | head -80
echo "exit=$rc"
