set -eu
ulimit -c 0
export CARGO_TARGET_DIR="$HOME/managed-proc-typed-target"
evidence="$HOME/managed-proc-typed-evidence/attach-v1"
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
git status --short > "$evidence/status.txt"
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
sha256sum crates/api/src/client.rs crates/api/src/taken.rs crates/api/src/lib.rs crates/api/src/conformance.rs docs/research/measurements/managed-proc-main/run-typed-attach.sh > "$evidence/source.sha256"
touch crates/api/src/client.rs crates/api/src/lib.rs crates/api/src/taken.rs crates/api/src/conformance.rs
printf '%s\n' 'cargo test --locked -j4 --profile test-release -p shards-api --target x86_64-unknown-linux-gnu --lib client::tests::attach_takes_back_a_guest_whose_ --message-format=json-render-diagnostics -- --test-threads=1 --nocapture' > "$evidence/native.command"
set +e
cargo test --locked -j4 --profile test-release -p shards-api --target x86_64-unknown-linux-gnu --lib client::tests::attach_takes_back_a_guest_whose_ --message-format=json-render-diagnostics -- --test-threads=1 --nocapture > "$evidence/native.log" 2>&1
result=$?
set -e
printf '%s\n' "$result" > "$evidence/native.exit"
python3 - "$evidence" <<'PY'
from pathlib import Path
import hashlib,json,shutil,sys
q=Path(sys.argv[1]);rows=[]
for line in (q/'native.log').read_text().splitlines():
 if line.startswith('{'):
  row=json.loads(line)
  if row.get('reason')=='compiler-artifact' and row.get('executable'):
   p=Path(row['executable']);rows.append({'path':str(p),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'fresh':row.get('fresh')})
assert len(rows)==1 and rows[0]['fresh'] is False,rows
(q/'binary.json').write_text(json.dumps(rows[0],indent=2)+'\n')
shutil.copyfile(rows[0]['path'],q/'test.bin')
PY
tar --exclude='*.bin' -czf "$HOME/managed-proc-typed-evidence/attach-v1-raw.tgz" -C "$evidence" .
printf 'focused_attach: exit=%s\n' "$result"
test "$result" -eq 0
