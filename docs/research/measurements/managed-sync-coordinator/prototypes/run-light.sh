set -eu
ulimit -c 0
probe_source=docs/research/measurements/managed-sync-coordinator/prototypes
probe_root="$HOME/managed-pthread-probe-check"
evidence="$HOME/managed-pthread-probe-evidence/v1"
test ! -e "$probe_root"
test ! -e "$evidence"
test ! -e "$HOME/managed-pthread-probe-target"
mkdir -p "$evidence" "$probe_root/$probe_source" "$probe_root/docs/research/measurements/managed-borrowed"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
sw_vers > "$evidence/sw-vers.txt"
df -k "$HOME" > "$evidence/df-before.txt"
test "$(df -k "$HOME" | awk 'END { print $4 }')" -ge 31457280
test "$(uname -m)" = arm64
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
xcrun --show-sdk-path > "$evidence/sdk-path.txt"
sdk="$(cat "$evidence/sdk-path.txt")"
shasum -a 256 "$sdk/usr/include/sys/_pthread/_pthread_types.h" > "$evidence/sdk-pthread-types.sha256"
cp -R "$probe_source/." "$probe_root/$probe_source/"
cp docs/research/measurements/managed-borrowed/allocation.rs "$probe_root/docs/research/measurements/managed-borrowed/allocation.rs"
cp rust-toolchain.toml "$probe_root/rust-toolchain.toml"
shasum -a 256 "$probe_source/src/main.rs" "$probe_source/Cargo.toml" "$probe_source/Cargo.lock" docs/research/measurements/managed-borrowed/allocation.rs rust-toolchain.toml > "$evidence/source.sha256"
python3 - <<'PY' > "$evidence/outer-wait-ns.txt"
from pathlib import Path
import re
s=Path('crates/api/src/conformance.rs').read_text()
scope=s[s.index('pub static PATIENCE:'):]
match=re.search(r'\("macOS-HVF", Duration::from_nanos\(([\d_]+)\)\)',scope)
assert match
# Existing Learned's cold bound: twice the recorded maximum; no new timeout default.
print(2*int(match[1].replace('_','')))
PY
export CARGO_TARGET_DIR="$HOME/managed-pthread-probe-target"
cd "$probe_root/$probe_source"
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
run_check build cargo build --locked -j4 --release --message-format=json-render-diagnostics
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
  binary="$CARGO_TARGET_DIR/release/darwin-pthread-probe"
  run_check allocation-refused "$binary" allocation-refused "$(cat "$evidence/outer-wait-ns.txt")"
  run_check native-under-rust-refusal "$binary" native-under-rust-refusal "$(cat "$evidence/outer-wait-ns.txt")"
fi
run_check clippy cargo clippy --locked -j4 --all-targets -- -D warnings
df -k "$HOME" > "$evidence/df-after.txt"
tar -czf "$HOME/managed-pthread-probe-evidence/v1-raw.tgz" -C "$evidence" .
test "$(cat "$evidence/build.exit")" -eq 0
test "$(cat "$evidence/allocation-refused.exit")" -eq 0
test "$(cat "$evidence/native-under-rust-refusal.exit")" -eq 0
test "$(cat "$evidence/clippy.exit")" -eq 0
