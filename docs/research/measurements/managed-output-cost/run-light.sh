set -eu
ulimit -c 0
phase="$1"
probe="docs/research/measurements/managed-output-cost"
export CARGO_TARGET_DIR="$HOME/managed-output-cost-target"
records="$HOME/managed-output-cost-light/$phase"
test ! -e "$records"
mkdir -p "$records" "$CARGO_TARGET_DIR"
git rev-parse HEAD > "$records/source.txt"
git status --short > "$records/status.txt"
uname -a > "$records/uname.txt"
rustc -Vv > "$records/rustc.txt"
cargo -Vv > "$records/cargo.txt"
sha256sum crates/shards/tests/managed_output.rs "$probe/fixture.rs" "$probe/parse.rs" "$probe/run-light.sh" Cargo.lock > "$records/source.sha256"
run_check() {
  local name="$1"; shift
  { printf '%q' "$1"; for arg in "${@:2}"; do printf ' %q' "$arg"; done; printf '\n'; } > "$records/$name.command"
  set +e
  "$@" > "$records/$name.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$records/$name.exit"
  printf '%s: exit=%s\n' "$name" "$result"
}
# This binary contains only synthetic parser tests; no host observations or VM calls.
run_check parser-build rustc --edition 2024 --test -D warnings "$probe/parse.rs" -o "$CARGO_TARGET_DIR/parser-tests-$phase"
if test "$(cat "$records/parser-build.exit")" -eq 0; then
  sha256sum "$CARGO_TARGET_DIR/parser-tests-$phase" > "$records/parser-binary.sha256"
  run_check parser "$CARGO_TARGET_DIR/parser-tests-$phase" --nocapture
fi
# Full actual-source cargo check is coordinator-owed: shards/build.rs requires
# real carried init/server artifacts or nested guest builds outside this light run.
printf '%s\n' 'OWED: actual-source SHARDS_HELPERS=skip cargo check -p shards --test managed_output --offline --locked -j4' > "$records/full-compile.owed"
tar -czf "$HOME/managed-output-cost-light/$phase-raw.tgz" -C "$records" .
for check in parser-build parser; do
  test -f "$records/$check.exit"
  test "$(cat "$records/$check.exit")" -eq 0
done
