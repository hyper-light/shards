set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-reconnect-light-01a1c5d2-v2"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,tarfile
root=Path.home()/'managed-tenant-reconnect-light-01a1c5d2-v2'
data=base64.b64decode('<base64 source SHA256 d0b22b20444abcf37165b4706d051ba088454715ddd4b3d74c0eef6e7f70a43a>')
assert hashlib.sha256(data).hexdigest()=='d0b22b20444abcf37165b4706d051ba088454715ddd4b3d74c0eef6e7f70a43a'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:tar.extractall(root/'source',filter='data')
(root/'source-transfer.sha256').write_text('d0b22b20444abcf37165b4706d051ba088454715ddd4b3d74c0eef6e7f70a43a\n')
PY
cp -R "$HOME/managed-tenant-crash-light-01a1c5d2-v5/source/policy" "$task_root/source/policy"
cd "$task_root/source"
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="$task_root/target"
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
cargo -Vv > "$task_root/cargo.txt"
rustfmt --edition 2024 --config-path rustfmt.toml --config skip_children=true original/tenant_exit.rs original/tenant_reconnect.rs original/tenant_observer.rs > "$task_root/format.log" 2>&1
cp original/tenant_exit.rs src/tenant_exit.rs
cp original/tenant_reconnect.rs src/tenant_reconnect.rs
cp original/tenant_observer.rs src/tenant_observer.rs
set +e
cargo check --tests --offline -j1 > "$task_root/check.log" 2>&1
check=$?
cargo clippy --tests --offline -j1 -- -D warnings > "$task_root/clippy.log" 2>&1
clippy=$?
cargo check --tests --offline -j1 --target x86_64-unknown-linux-musl > "$task_root/musl-check.log" 2>&1
musl_check=$?
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings > "$task_root/musl-clippy.log" 2>&1
musl_clippy=$?
cargo test --lib --offline -j1 tenant_exit::reconnect:: -- --skip observer_disconnect_replays_retained_record_without_losing_owner --nocapture --test-threads=1 > "$task_root/pure-test.log" 2>&1
pure_test=$?
cargo test --lib --offline -j1 --target x86_64-unknown-linux-musl tenant_exit::reconnect:: -- --skip observer_disconnect_replays_retained_record_without_losing_owner --nocapture --test-threads=1 > "$task_root/musl-pure-test.log" 2>&1
musl_pure_test=$?
python3 policy/scripts/check-development-policy > "$task_root/policy.log" 2>&1
policy=$?
set -e
for item in check clippy musl_check musl_clippy pure_test musl_pure_test policy; do eval "printf '%s\\n' \"\$$item\" > \"$task_root/$item.exit\""; done
du -sk "$task_root/source" "$task_root/target" > "$task_root/disk.txt"
python3 - <<'PY'
from pathlib import Path
import json,hashlib
r=Path.home()/'managed-tenant-reconnect-light-01a1c5d2-v2'
(r/'checked-hashes.json').write_text(json.dumps({str(p.relative_to(r/'source')):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((r/'source').rglob('*')) if p.is_file()},indent=2)+'\n')
print(json.dumps({p.name:p.read_text() for p in r.glob('*.exit')}))
for n in ['check.log','clippy.log','musl-check.log','musl-clippy.log','pure-test.log','musl-pure-test.log','policy.log','disk.txt']:print(n+'\n'+(r/n).read_text())
PY
test "$check" = 0 && test "$clippy" = 0 && test "$musl_check" = 0 && test "$musl_clippy" = 0 && test "$pure_test" = 0 && test "$musl_pure_test" = 0 && test "$policy" = 0
