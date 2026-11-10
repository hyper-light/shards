set -eu
probe=docs/research/measurements/managed-proc-reader-coordinator/prototypes
export CARGO_TARGET_DIR="$HOME/managed-proc-blksize-target"
evidence="$HOME/managed-proc-blksize-evidence/v1"
test ! -e "$CARGO_TARGET_DIR"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum "$probe/src/lib.rs" "$probe/Cargo.toml" "$probe/Cargo.lock" docs/research/measurements/managed-borrowed/allocation.rs crates/testdir/src/lib.rs > "$evidence/source.sha256"
run_check() {
  name="$1"
  shift
  printf '%q ' "$@" > "$evidence/$name.command"
  printf '\n' >> "$evidence/$name.command"
  set +e
  "$@" > "$evidence/$name.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$evidence/$name.exit"
  printf '%s: exit=%s\n' "$name" "$result"
}
run_check build cargo test --manifest-path "$probe/Cargo.toml" --locked -j4 --release --no-run --message-format=json-render-diagnostics
if test "$(cat "$evidence/build.exit")" -eq 0; then
  python3 - "$evidence/build.log" "$evidence/binary.json" <<'PY'
from pathlib import Path
import hashlib,json,sys
rows=[]
for line in Path(sys.argv[1]).read_text().splitlines():
 if line.startswith('{'):
  row=json.loads(line)
  if row.get('reason')=='compiler-artifact' and row.get('executable'):
   path=Path(row['executable']);rows.append({'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'fresh':row.get('fresh')})
assert len(rows)==1 and rows[0]['fresh'] is False,rows
Path(sys.argv[2]).write_text(json.dumps(rows[0],indent=2)+'\n')
PY
  binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/binary.json")"
  run_check tests "$binary" --test-threads=1 --nocapture
fi
run_check clippy cargo clippy --manifest-path "$probe/Cargo.toml" --locked -j4 --all-targets -- -D warnings
git diff --exit-code -- crates/api > "$evidence/production-api.diff"
tar -czf "$HOME/managed-proc-blksize-evidence/v1-raw.tgz" -C "$evidence" .
test "$(cat "$evidence/build.exit")" -eq 0
test "$(cat "$evidence/tests.exit")" -eq 0
test "$(cat "$evidence/clippy.exit")" -eq 0
