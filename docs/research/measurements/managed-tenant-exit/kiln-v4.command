set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-exit-light-24f-v4"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,tarfile
root=Path.home()/'managed-tenant-exit-light-24f-v4'
data=base64.b64decode('<base64 archive; SHA256 f078ad194cef600fb9a0fbacf22d5cc12cf3244302b7b9100453b5bb92d2a107>')
assert hashlib.sha256(data).hexdigest()=='f078ad194cef600fb9a0fbacf22d5cc12cf3244302b7b9100453b5bb92d2a107'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:tar.extractall(root/'source',filter='data')
(root/'source-transfer.sha256').write_text('f078ad194cef600fb9a0fbacf22d5cc12cf3244302b7b9100453b5bb92d2a107\n')
PY
cd "$task_root/source"
export CARGO_TARGET_DIR="$task_root/target"
export CARGO_BUILD_JOBS=1
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
cargo -Vv > "$task_root/cargo.txt"
set +e
cargo check --tests --offline -j1 > "$task_root/check.log" 2>&1
check=$?
cargo clippy --tests --offline -j1 -- -D warnings > "$task_root/clippy.log" 2>&1
clippy=$?
cargo check --tests --offline -j1 --target x86_64-unknown-linux-musl > "$task_root/musl-check.log" 2>&1
musl_check=$?
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings > "$task_root/musl-clippy.log" 2>&1
musl_clippy=$?
set -e
for item in check clippy musl_check musl_clippy; do eval "printf '%s\\n' \"\$$item\" > \"$task_root/$item.exit\""; done
cat "$task_root/check.log" "$task_root/clippy.log" "$task_root/musl-check.log" "$task_root/musl-clippy.log"
du -sk "$task_root/source" "$task_root/target"
test "$check" -eq 0
test "$clippy" -eq 0
test "$musl_check" -eq 0
test "$musl_clippy" -eq 0
