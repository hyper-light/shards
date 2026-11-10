set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-publish-light-01a1c5d2-v1"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path
import base64,hashlib,io,tarfile
root=Path.home()/'managed-tenant-publish-light-01a1c5d2-v1'
data=base64.b64decode('<base64 source SHA256 0a37464c7f17208ee3afc9d2999d3f40775f308d70e619e701d4f7c15f0c942e>')
assert hashlib.sha256(data).hexdigest()=='0a37464c7f17208ee3afc9d2999d3f40775f308d70e619e701d4f7c15f0c942e'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:tar.extractall(root/'source',filter='data')
(root/'source-transfer.sha256').write_text('0a37464c7f17208ee3afc9d2999d3f40775f308d70e619e701d4f7c15f0c942e\n')
PY
cp -R "$HOME/managed-tenant-crash-light-01a1c5d2-v5/source/policy" "$task_root/source/policy"
cd "$task_root/source"
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="$task_root/target"
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
cargo -Vv > "$task_root/cargo.txt"
rustfmt --edition 2024 --config-path rustfmt.toml --config skip_children=true original/managed_output.rs original/tenant_exit.rs src/publication.rs > "$task_root/format.log" 2>&1
cp original/tenant_exit.rs src/tenant_exit.rs
set +e
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
for item in check clippy musl_check musl_clippy policy; do eval "printf '%s\\n' \"\$$item\" > \"$task_root/$item.exit\""; done
du -sk "$task_root/source" "$task_root/target" > "$task_root/disk.txt"
python3 - <<'PY'
from pathlib import Path
import json,hashlib
r=Path.home()/'managed-tenant-publish-light-01a1c5d2-v1'
(r/'checked-hashes.json').write_text(json.dumps({str(p.relative_to(r/'source')):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((r/'source').rglob('*')) if p.is_file()},indent=2)+'\n')
print(json.dumps({p.name:p.read_text() for p in r.glob('*.exit')}))
for n in ['check.log','clippy.log','musl-check.log','musl-clippy.log','policy.log','disk.txt']:print(n+'\n'+(r/n).read_text())
PY
test "$check" = 0 && test "$clippy" = 0 && test "$musl_check" = 0 && test "$musl_clippy" = 0 && test "$policy" = 0
