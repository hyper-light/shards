set -eu
phase="$1"
export CARGO_TARGET_DIR="$HOME/managed-proc-main-target"
evidence="$HOME/managed-proc-main-evidence/$phase"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/taken.rs crates/api/src/lib.rs docs/research/measurements/managed-proc-main/*.rs > "$evidence/source.sha256"
run_check() {
  name="$1"; shift
  printf '%q ' "$@" > "$evidence/$name.command"; printf '\n' >> "$evidence/$name.command"
  set +e
  "$@" > "$evidence/$name.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$evidence/$name.exit"
  printf '%s: exit=%s\n' "$name" "$result"
}
touch crates/api/src/taken.rs crates/api/src/lib.rs docs/research/measurements/managed-proc-main/*.rs
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
  run_check tests "$binary" taken::linux::tests:: --test-threads=1 --nocapture
fi
run_check clippy cargo clippy --locked -j4 -p shards-api --all-targets -- -D warnings
tar --exclude='*.bin' -czf "$HOME/managed-proc-main-evidence/$phase-raw.tgz" -C "$evidence" .
test "$(cat "$evidence/build.exit")" -eq 0
test "$(cat "$evidence/tests.exit")" -eq 0
test "$(cat "$evidence/clippy.exit")" -eq 0
