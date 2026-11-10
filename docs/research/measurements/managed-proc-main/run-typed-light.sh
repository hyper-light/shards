set -eu
ulimit -c 0
phase="$1"
export CARGO_TARGET_DIR="$HOME/managed-proc-typed-target"
evidence="$HOME/managed-proc-typed-evidence/$phase"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
rustup target list --installed > "$evidence/installed-targets.txt"
sha256sum crates/api/src/taken.rs crates/api/src/lib.rs crates/api/src/client.rs crates/api/src/conformance.rs docs/research/measurements/managed-proc-main/*.rs docs/research/measurements/managed-proc-main/run-typed-light.sh > "$evidence/source.sha256"
run_check() {
  local name="$1"; shift
  { printf '%q' "$1"; for arg in "${@:2}"; do printf ' %q' "$arg"; done; printf '\n'; } > "$evidence/$name.command"
  set +e
  "$@" > "$evidence/$name.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$evidence/$name.exit"
  printf '%s: exit=%s\n' "$name" "$result"
}
build_test() {
  local name="$1" triple="$2"
  run_check "$name-build" cargo test --locked -j4 --profile test-release -p shards-api --target "$triple" --lib --no-run --message-format=json-render-diagnostics
  if test "$(cat "$evidence/$name-build.exit")" -eq 0; then
    python3 - "$evidence/$name-build.log" "$evidence/$name-binary.json" <<'PY'
from pathlib import Path
import hashlib,json,sys
rows=[]
for line in Path(sys.argv[1]).read_text().splitlines():
 if line.startswith('{'):
  row=json.loads(line)
  if row.get('reason')=='compiler-artifact' and row.get('executable'):
   p=Path(row['executable']);rows.append({'path':str(p),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'fresh':row.get('fresh')})
assert len(rows)==1 and rows[0]['fresh'] is False,rows
Path(sys.argv[2]).write_text(json.dumps(rows[0],indent=2)+'\n')
PY
    binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/$name-binary.json")"
    cp "$binary" "$evidence/$name-test.bin"
    run_check "$name-tests" "$binary" taken::linux::tests:: --test-threads=1 --nocapture
  fi
  run_check "$name-clippy" cargo clippy --locked -j4 -p shards-api --target "$triple" --all-targets -- -D warnings
}
touch crates/api/src/taken.rs crates/api/src/lib.rs crates/api/src/client.rs crates/api/src/conformance.rs docs/research/measurements/managed-proc-main/*.rs
build_test gnu x86_64-unknown-linux-gnu
build_test musl x86_64-unknown-linux-musl
run_check windows-clippy cargo clippy --locked -j4 -p shards-api --target x86_64-pc-windows-msvc --all-targets -- -D warnings
tar --exclude='*.bin' -czf "$HOME/managed-proc-typed-evidence/$phase-raw.tgz" -C "$evidence" .
for check in gnu-build gnu-tests gnu-clippy musl-build musl-tests musl-clippy windows-clippy; do
  test -f "$evidence/$check.exit"
  test "$(cat "$evidence/$check.exit")" -eq 0
done
