set -eu
ulimit -c 0
phase="$1"
probe="docs/research/measurements/managed-proc-main/public-probe"
export CARGO_TARGET_DIR="$HOME/managed-proc-public-target"
evidence="$HOME/managed-proc-public-evidence/$phase"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/*.rs "$probe/Cargo.toml" "$probe/Cargo.lock" "$probe/src/main.rs" "$probe/run-light.sh" docs/research/measurements/managed-proc-main/allocation.rs > "$evidence/source.sha256"
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
touch crates/api/src/*.rs "$probe/src/main.rs" docs/research/measurements/managed-proc-main/allocation.rs
run_check build cargo build --manifest-path "$probe/Cargo.toml" --offline --locked --release -j4 --message-format=json-render-diagnostics
if test "$(cat "$evidence/build.exit")" -eq 0; then
  python3 - "$evidence" <<'PY'
from pathlib import Path
import hashlib,json,shutil,sys
q=Path(sys.argv[1]);rows=[]
for line in (q/'build.log').read_text().splitlines():
 if line.startswith('{'):
  row=json.loads(line)
  if row.get('reason')=='compiler-artifact' and row.get('executable'):
   p=Path(row['executable']);rows.append({'path':str(p),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'fresh':row.get('fresh')})
assert len(rows)==1 and rows[0]['fresh'] is False,rows
(q/'binary.json').write_text(json.dumps(rows[0],indent=2)+'\n')
shutil.copyfile(rows[0]['path'],q/'probe.bin')
PY
  binary="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$evidence/binary.json")"
  run_check oom "$binary" oom
  run_check raw-comm "$binary" raw-comm
fi
run_check clippy cargo clippy --manifest-path "$probe/Cargo.toml" --offline --locked -j4 -- -D warnings
tar --exclude='*.bin' -czf "$HOME/managed-proc-public-evidence/$phase-raw.tgz" -C "$evidence" .
for check in build oom raw-comm clippy; do
  test -f "$evidence/$check.exit"
  test "$(cat "$evidence/$check.exit")" -eq 0
done
