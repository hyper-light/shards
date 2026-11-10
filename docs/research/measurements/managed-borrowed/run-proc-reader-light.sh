set -eu
ulimit -c 0
export CARGO_TARGET_DIR="$HOME/managed-borrowed-comm-target"
evidence="$HOME/managed-borrowed-evidence/proc-reader-v1"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/taken.rs crates/api/src/borrowed.rs docs/research/measurements/managed-borrowed/proc-comm.rs docs/research/measurements/managed-borrowed/proc-stat.rs > "$evidence/source.sha256"
cp crates/api/src/taken.rs "$evidence/taken.saved"
restore() {
  cp "$evidence/taken.saved" crates/api/src/taken.rs
  touch crates/api/src/taken.rs
}
trap restore EXIT
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
build() {
  phase="$1"
  touch crates/api/src/*.rs docs/research/measurements/managed-borrowed/*.rs
  run_check "$phase-build" cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api --lib --no-run --message-format=json-render-diagnostics
  test "$(cat "$evidence/$phase-build.exit")" -eq 0
  python3 - "$evidence/$phase-build.log" "$evidence/$phase-binary.json" <<'PY'
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
  binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/$phase-binary.json")"
  cp "$binary" "$evidence/$phase.bin"
}
test_name=taken::linux::tests::proc_reader_and_start_path_return_allocation_failure
build fixed
run_check reader "$binary" taken::linux::tests:: --test-threads=1 --nocapture
test "$(cat "$evidence/reader.exit")" -eq 0
run_check comm "$binary" --exact borrowed::proc_comm::non_utf8_comm_keeps_held_identity_and_borrowed_connect --test-threads=1 --nocapture
test "$(cat "$evidence/comm.exit")" -eq 0
python3 - "$evidence" <<'PY'
import subprocess,sys
from pathlib import Path
path=Path('crates/api/src/taken.rs');source=path.read_text()
old='bytes\n                .try_reserve(1)\n                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;'
assert source.count(old)==1
path.write_text(source.replace(old,'bytes.reserve(1);'))
(Path(sys.argv[1])/'infallible-growth.patch').write_bytes(subprocess.check_output(['git','diff','--','crates/api/src/taken.rs']))
PY
build infallible-growth
run_check infallible-growth "$binary" --exact "$test_name" --test-threads=1 --nocapture
restore
test "$(cat "$evidence/infallible-growth.exit")" -eq 134
python3 - "$evidence/infallible-growth.log" <<'PY'
from pathlib import Path
import sys
assert 'memory allocation of' in Path(sys.argv[1]).read_text()
PY
git diff --exit-code -- crates/api/src/taken.rs > "$evidence/restored.diff"
build restored
run_check restored "$binary" --exact "$test_name" --test-threads=1 --nocapture
test "$(cat "$evidence/restored.exit")" -eq 0
run_check corpus cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api -- --test-threads=1 --nocapture
run_check clippy cargo clippy --locked -j4 --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix --all-targets -- -D warnings
sha256sum crates/api/src/taken.rs crates/api/src/borrowed.rs docs/research/measurements/managed-borrowed/proc-comm.rs docs/research/measurements/managed-borrowed/proc-stat.rs > "$evidence/final-source.sha256"
diff "$evidence/source.sha256" "$evidence/final-source.sha256" > "$evidence/final-source.diff"
tar --exclude='*.bin' -czf "$HOME/managed-borrowed-evidence/proc-reader-v1-raw.tgz" -C "$evidence" .
test "$(cat "$evidence/corpus.exit")" -eq 0
test "$(cat "$evidence/clippy.exit")" -eq 0
