set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-crash-light-01a1c5d2-v5"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,tarfile
root=Path.home()/'managed-tenant-crash-light-01a1c5d2-v5'
data=base64.b64decode('<base64 archive SHA256 1201733942a4ca7068753c98deab58a352181f392fddf2b2bf1f552b3f69384a>')
assert hashlib.sha256(data).hexdigest()=='1201733942a4ca7068753c98deab58a352181f392fddf2b2bf1f552b3f69384a'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:tar.extractall(root/'source',filter='data')
(root/'source-transfer.sha256').write_text('1201733942a4ca7068753c98deab58a352181f392fddf2b2bf1f552b3f69384a\n')
PY
cd "$task_root/source"
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="$task_root/target"
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
cargo -Vv > "$task_root/cargo.txt"
set +e
rustfmt --edition 2024 --config-path rustfmt.toml --config skip_children=true src/helper.rs original/managed_output.rs original/tenant_exit.rs > "$task_root/format.log" 2>&1
format=$?
cp original/tenant_exit.rs src/tenant_exit.rs
cargo check --tests --offline -j1 > "$task_root/check.log" 2>&1
check=$?
cargo clippy --tests --offline -j1 -- -D warnings > "$task_root/clippy.log" 2>&1
clippy=$?
cargo check --tests --offline -j1 --target x86_64-unknown-linux-musl > "$task_root/musl-check.log" 2>&1
musl_check=$?
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings > "$task_root/musl-clippy.log" 2>&1
musl_clippy=$?
python3 policy/scripts/check-development-policy > "$task_root/policy.log" 2>&1
policy=$?
set -e
for item in format check clippy musl_check musl_clippy policy; do eval "printf '%s\\n' \"\$$item\" > \"$task_root/$item.exit\""; done
du -sk "$task_root/source" "$task_root/target" > "$task_root/disk.txt"
python3 - <<'PY'
from pathlib import Path
import json
root=Path.home()/'managed-tenant-crash-light-01a1c5d2-v5'
import hashlib
(root/'checked-hashes.json').write_text(json.dumps({str(p.relative_to(root/'source')):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'source').rglob('*')) if p.is_file()},indent=2)+'\n')
print(json.dumps({p.name:p.read_text() for p in sorted(root.glob('*.exit'))}))
print((root/'disk.txt').read_text())
for name in ['format.log','check.log','clippy.log','musl-check.log','musl-clippy.log','policy.log']:
 print(name);print((root/name).read_text())
PY
test "$format" = 0 && test "$check" = 0 && test "$clippy" = 0 && test "$musl_check" = 0 && test "$musl_clippy" = 0 && test "$policy" = 0
