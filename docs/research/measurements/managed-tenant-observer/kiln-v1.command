set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-observer-pure-01a1c5d2-v1"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,tarfile
root=Path.home()/'managed-tenant-observer-pure-01a1c5d2-v1'
data=base64.b64decode('<base64 archive SHA256 d5f7080e3ced2766b975e8f8a67d87117e11ec65fbac694c173ff880db9479b0>')
assert hashlib.sha256(data).hexdigest()=='d5f7080e3ced2766b975e8f8a67d87117e11ec65fbac694c173ff880db9479b0'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:tar.extractall(root/'source',filter='data')
(root/'source-transfer.sha256').write_text('d5f7080e3ced2766b975e8f8a67d87117e11ec65fbac694c173ff880db9479b0\n')
PY
cd "$task_root/source"
export CARGO_TARGET_DIR="$task_root/target"
export CARGO_BUILD_JOBS=1
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
cargo -Vv > "$task_root/cargo.txt"
rustfmt --edition 2024 --config-path rustfmt.toml src/tenant_reconnect.rs > "$task_root/format.log" 2>&1
set +e
cargo test --lib --offline -j1 -- --nocapture --test-threads=1 > "$task_root/test.log" 2>&1
test_gnu=$?
cargo clippy --tests --offline -j1 -- -D warnings > "$task_root/clippy.log" 2>&1
clippy=$?
cargo test --lib --offline -j1 --target x86_64-unknown-linux-musl -- --nocapture --test-threads=1 > "$task_root/musl-test.log" 2>&1
test_musl=$?
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings > "$task_root/musl-clippy.log" 2>&1
clippy_musl=$?
set -e
for item in test_gnu clippy test_musl clippy_musl; do eval "printf '%s\\n' \"\$$item\" > \"$task_root/$item.exit\""; done
du -sk "$task_root/source" "$task_root/target" > "$task_root/disk.txt"
python3 - <<'PY'
from pathlib import Path
import hashlib,json
root=Path.home()/'managed-tenant-observer-pure-01a1c5d2-v1'
(root/'checked-hashes.json').write_text(json.dumps({str(p.relative_to(root/'source')):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'source').rglob('*')) if p.is_file()},indent=2)+'\n')
print(json.dumps({p.name:p.read_text() for p in root.glob('*.exit')}))
for name in ['test.log','clippy.log','musl-test.log','musl-clippy.log','disk.txt']: print(name+'\n'+(root/name).read_text())
PY
test "$test_gnu" = 0 && test "$clippy" = 0 && test "$test_musl" = 0 && test "$clippy_musl" = 0
