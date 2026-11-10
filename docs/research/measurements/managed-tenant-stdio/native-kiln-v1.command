set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-stdio-native-light-01a1c5d2-v1"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,tarfile
r=Path.home()/'managed-tenant-stdio-native-light-01a1c5d2-v1'
data=base64.b64decode('<base64 source SHA256 be1f021d28e36a63fb6263b55547a24a9d73836abb1345cbba7df4a54b098ea8>')
assert hashlib.sha256(data).hexdigest()=='be1f021d28e36a63fb6263b55547a24a9d73836abb1345cbba7df4a54b098ea8'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:tar.extractall(r/'source',filter='data')
PY
cp -R "$HOME/managed-tenant-crash-light-01a1c5d2-v5/source/policy" "$task_root/source/policy"
cd "$task_root/source"
export CARGO_TARGET_DIR="$task_root/target" CARGO_BUILD_JOBS=1
rustfmt --edition 2024 --config-path rustfmt.toml src/helper.rs
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
set +e
cargo check --lib --offline -j1 > "$task_root/check.log" 2>&1
check=$?
cargo clippy --lib --offline -j1 -- -D warnings > "$task_root/clippy.log" 2>&1
clippy=$?
cargo check --lib --offline -j1 --target x86_64-unknown-linux-musl > "$task_root/musl-check.log" 2>&1
musl_check=$?
cargo clippy --lib --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings > "$task_root/musl-clippy.log" 2>&1
musl_clippy=$?
python3 policy/scripts/check-development-policy > "$task_root/policy.log" 2>&1
policy=$?
set -e
python3 - <<PY
from pathlib import Path
import json,hashlib
r=Path.home()/'managed-tenant-stdio-native-light-01a1c5d2-v1'
v={'check':$check,'clippy':$clippy,'musl_check':$musl_check,'musl_clippy':$musl_clippy,'policy':$policy}
v['helper_sha256']=hashlib.sha256((r/'source/src/helper.rs').read_bytes()).hexdigest()
(r/'results.json').write_text(json.dumps(v,indent=2)+'\n');print(json.dumps(v))
for p in r.glob('*.log'): print(p.name+'\n'+p.read_text())
PY
test "$check" = 0 && test "$clippy" = 0 && test "$musl_check" = 0 && test "$musl_clippy" = 0 && test "$policy" = 0
