set -eu
ulimit -c 0
evidence="$HOME/managed-borrowed-evidence/proc-comm-followup"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
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
bash docs/research/measurements/managed-borrowed/run-proc-comm-light.sh fixed
binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$HOME/managed-borrowed-evidence/proc-comm-fixed/binary.json")"
run_check fields "$binary" taken::linux::tests:: --skip proc_read_and_start_path_return_allocation_failure --test-threads=1 --nocapture
run_check oom "$binary" --exact taken::linux::tests::proc_read_and_start_path_return_allocation_failure --test-threads=1 --nocapture
python3 - "$evidence" <<'PY'
import subprocess,sys
from pathlib import Path
path=Path('crates/api/src/taken.rs')
source=path.read_text()
old='let stat = std::fs::read(&at)?;'
assert source.count(old)==1
path.write_text(source.replace(old,'let stat = std::fs::read_to_string(&at)?.into_bytes();'))
evidence=Path(sys.argv[1])
(evidence/'utf8-removal.patch').write_bytes(subprocess.check_output(['git','diff','--','crates/api/src/taken.rs']))
PY
set +e
bash docs/research/measurements/managed-borrowed/run-proc-comm-light.sh utf8-removal
mutation=$?
set -e
restore
test "$mutation" -eq 101
python3 - "$HOME/managed-borrowed-evidence/proc-comm-utf8-removal/test.log" <<'PY'
import sys
from pathlib import Path
log=Path(sys.argv[1]).read_text()
assert 'actual_CONNECT=false' in log and 'phase: Start' in log and 'InvalidData' in log,log
assert 'own_comm_restored=true start_unchanged=true child_exit=Some(0) workers_joined=true' in log,log
PY
git diff --exit-code -- crates/api/src/taken.rs > "$evidence/restored.diff"
bash docs/research/measurements/managed-borrowed/run-proc-comm-light.sh restored
export CARGO_TARGET_DIR="$HOME/managed-borrowed-comm-target"
run_check clippy cargo clippy --locked -j4 --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix --all-targets -- -D warnings
if test "$(cat "$evidence/oom.exit")" -eq 0; then
  run_check corpus cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api -- --test-threads=1 --nocapture
else
  printf '%s\n' 'Full ordinary corpus not rerun after the isolated OOM red; no skipped-to-green corpus claim.' > "$evidence/corpus-unrun.txt"
fi
sha256sum crates/api/src/taken.rs crates/api/src/borrowed.rs docs/research/measurements/managed-borrowed/proc-comm.rs docs/research/measurements/managed-borrowed/proc-stat.rs > "$evidence/final-source.sha256"
tar --exclude='*.bin' -czf "$HOME/managed-borrowed-evidence/proc-comm-followup-raw.tgz" -C "$evidence" .
test "$(cat "$evidence/fields.exit")" -eq 0
test "$(cat "$evidence/clippy.exit")" -eq 0
test "$(cat "$evidence/oom.exit")" -eq 0
test "$(cat "$evidence/corpus.exit")" -eq 0
