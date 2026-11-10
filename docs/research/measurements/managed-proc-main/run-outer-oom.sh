set -eu
ulimit -c 0
export CARGO_TARGET_DIR="$HOME/managed-proc-main-outer-oom-target"
evidence="$HOME/managed-proc-main-outer-oom-evidence/v1"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/taken.rs crates/api/src/lib.rs docs/research/measurements/managed-proc-main/reader-tests.rs docs/research/measurements/managed-proc-main/allocation.rs docs/research/measurements/managed-proc-main/run-outer-oom.sh > "$evidence/source.sha256"
run_check() {
  name="$1"; shift
  { printf '%q' "$1"; for arg in "${@:2}"; do printf ' %q' "$arg"; done; printf '\n'; } > "$evidence/$name.command"
  set +e
  "$@" > "$evidence/$name.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$evidence/$name.exit"
  printf '%s: exit=%s\n' "$name" "$result"
}
touch crates/api/src/taken.rs crates/api/src/lib.rs docs/research/measurements/managed-proc-main/reader-tests.rs docs/research/measurements/managed-proc-main/allocation.rs
run_check build cargo test --locked -j4 --profile test-release -p shards-api --lib --no-run --message-format=json-render-diagnostics
if test "$(cat "$evidence/build.exit")" -eq 0; then
  python3 - "$evidence/build.log" "$evidence/binary.json" <<'PY'
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
  binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/binary.json")"
  cp "$binary" "$evidence/test.bin"
  run_check test "$binary" --exact taken::linux::tests::held_find_returns_error_when_start_allocation_is_refused --test-threads=1 --nocapture
fi
run_check clippy cargo clippy --locked -j4 -p shards-api --all-targets -- -D warnings
tar --exclude='*.bin' -czf "$HOME/managed-proc-main-outer-oom-evidence/v1-raw.tgz" -C "$evidence" .
test "$(cat "$evidence/build.exit")" -eq 0
test "$(cat "$evidence/test.exit")" -eq 0
test "$(cat "$evidence/clippy.exit")" -eq 0
