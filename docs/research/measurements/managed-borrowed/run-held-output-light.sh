set -eu
export CARGO_TARGET_DIR="$HOME/managed-borrowed-held-target"
batch="${HELD_OUTPUT_EVIDENCE:-held-output-v2}"
evidence="$HOME/managed-borrowed-evidence/$batch"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
git diff --exit-code -- crates/api crates/afunix Cargo.lock
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/*.rs crates/api/tests/sim.rs docs/research/measurements/managed-borrowed/held-output.rs > "$evidence/source.sha256"
cp crates/api/src/client.rs "$evidence/client.saved"
restore() {
  cp "$evidence/client.saved" crates/api/src/client.rs
  touch crates/api/src/client.rs
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
  touch crates/api/src/*.rs docs/research/measurements/managed-borrowed/held-output.rs
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
assert len(rows)==1,rows
assert rows[0]['fresh'] is False,rows
Path(sys.argv[2]).write_text(json.dumps(rows[0],indent=2)+'\n')
print(rows[0]['path'])
PY
  binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/$phase-binary.json")"
}
test_name=client::held_output::a_child_holding_shards_output_holds_no_refusal_back
build baseline
cp "$binary" "$evidence/baseline.bin"
run_check baseline "$binary" --exact "$test_name" --test-threads=1 --nocapture
test "$(cat "$evidence/baseline.exit")" -eq 0
python3 - "$evidence" <<'PY'
import subprocess,sys
from pathlib import Path
evidence=Path(sys.argv[1])
path=Path('crates/api/src/client.rs')
source=path.read_text()
old='if at.is_some() {\n                    break;\n                }'
assert source.count(old)==1
path.write_text(source.replace(old,'if false {\n                    break;\n                }'))
(evidence/'marker-removal.patch').write_bytes(subprocess.check_output(['git','diff','--','crates/api/src/client.rs']))
PY
sha256sum crates/api/src/client.rs > "$evidence/mutation-source.sha256"
build marker-removal
cp "$binary" "$evidence/marker-removal.bin"
run_check marker-removal "$binary" --exact "$test_name" --test-threads=1 --nocapture
restore
test "$(cat "$evidence/marker-removal.exit")" -eq 101
python3 - "$evidence/marker-removal.log" <<'PY'
import sys
from pathlib import Path
log=Path(sys.argv[1]).read_text()
assert 'refusal_before_writer_release=false writer_held_alive=true writer_unreaped=true' in log,log
assert 'Timeout' in log,log
PY
git diff --exit-code -- crates/api/src/client.rs > "$evidence/restored-diff.txt"
build restored
cp "$binary" "$evidence/restored.bin"
run_check restored "$binary" --exact "$test_name" --test-threads=1 --nocapture
test "$(cat "$evidence/restored.exit")" -eq 0
if test "${HELD_OUTPUT_FOCUSED_ONLY:-0}" -ne 1; then
  run_check corpus cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api -- --test-threads=1 --nocapture
  test "$(cat "$evidence/corpus.exit")" -eq 0
fi
run_check clippy cargo clippy --locked -j4 --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix --all-targets -- -D warnings
sha256sum crates/api/src/*.rs crates/api/tests/sim.rs docs/research/measurements/managed-borrowed/held-output.rs > "$evidence/final-source.sha256"
diff "$evidence/source.sha256" "$evidence/final-source.sha256" > "$evidence/final-source.diff"
test "$(cat "$evidence/clippy.exit")" -eq 0
tar --exclude='*.bin' -czf "$HOME/managed-borrowed-evidence/$batch-raw.tgz" -C "$evidence" .
