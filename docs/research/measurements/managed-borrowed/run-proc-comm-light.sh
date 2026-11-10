set -eu
phase="$1"
export CARGO_TARGET_DIR="$HOME/managed-borrowed-comm-target"
evidence="$HOME/managed-borrowed-evidence/proc-comm-$phase"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/taken.rs crates/api/src/borrowed.rs docs/research/measurements/managed-borrowed/proc-comm.rs > "$evidence/source.sha256"
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
touch crates/api/src/*.rs docs/research/measurements/managed-borrowed/*.rs
run_check build cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api --lib --no-run --message-format=json-render-diagnostics
if test "$(cat "$evidence/build.exit")" -eq 0; then
  python3 - "$evidence/build.log" "$evidence/binary.json" <<'PY'
import hashlib,json,sys
from pathlib import Path
rows=[]
for line in Path(sys.argv[1]).read_text().splitlines():
    if not line.startswith('{'):continue
    row=json.loads(line)
    if row.get('reason')=='compiler-artifact' and row.get('executable'):
        path=Path(row['executable'])
        rows.append({'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'fresh':row.get('fresh')})
assert len(rows)==1 and rows[0]['fresh'] is False,rows
Path(sys.argv[2]).write_text(json.dumps(rows[0],indent=2)+'\n')
PY
  binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/binary.json")"
  cp "$binary" "$evidence/test.bin"
  run_check test "$binary" --exact borrowed::proc_comm::non_utf8_comm_keeps_held_identity_and_borrowed_connect --test-threads=1 --nocapture
  result="$(cat "$evidence/test.exit")"
else
  result="$(cat "$evidence/build.exit")"
fi
git diff -- crates/api > "$evidence/final.diff"
tar --exclude='*.bin' -czf "$HOME/managed-borrowed-evidence/proc-comm-$phase-raw.tgz" -C "$evidence" .
exit "$result"
