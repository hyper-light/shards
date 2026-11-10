set -eu
ulimit -c 0
task_root="$HOME/managed-tenant-envelope-light-01a1c5d2-v1"
test ! -e "$task_root"
mkdir "$task_root"
python3 - <<'PY'
from pathlib import Path,PurePosixPath
import base64,hashlib,io,tarfile,json
root=Path.home()/'managed-tenant-envelope-light-01a1c5d2-v1'
# SOURCE_TRANSFER_BASE64
# Seven chunks of <=16000 base64 characters, decoded archive SHA256 5c3c0f4146bd56380390fd595a856c0b18e5c54845c0f92208f2e698c9411c50
# END_SOURCE_TRANSFER_BASE64
assert len(data)==75016
assert hashlib.sha256(data).hexdigest()=='5c3c0f4146bd56380390fd595a856c0b18e5c54845c0f92208f2e698c9411c50'
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:
 for member in tar.getmembers():
  path=PurePosixPath(member.name)
  assert not path.is_absolute() and '..' not in path.parts
  assert member.isfile() or member.isdir()
 tar.extractall(root/'source',filter='data')
source=root/'source'
manifest=json.loads((source/'TRANSFER-MEMBERS.json').read_text())
actual={str(p.relative_to(source)):hashlib.sha256(p.read_bytes()).hexdigest() for p in source.rglob('*') if p.is_file() and p.name!='TRANSFER-MEMBERS.json'}
assert actual==manifest and len(actual)==27
(root/'source-transfer.sha256').write_text('5c3c0f4146bd56380390fd595a856c0b18e5c54845c0f92208f2e698c9411c50\n')
(root/'transfer-verification.json').write_text(json.dumps({'archive_bytes':len(data),'members':len(actual),'regular_files_and_safe_paths':True,'hashes_match':True})+'\n')
PY
cp -R "$HOME/managed-tenant-crash-light-01a1c5d2-v5/source/policy" "$task_root/source/policy"
cd "$task_root/source"
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="$task_root/target"
export SHARDS_ENVELOPE_CHALLENGE_BYTES=16
uname -a > "$task_root/uname.txt"
rustc -Vv > "$task_root/rustc.txt"
cargo -Vv > "$task_root/cargo.txt"
rustfmt --edition 2024 --config-path rustfmt.toml --config skip_children=true original/managed_output.rs original/tenant_exit.rs original/tenant_stdio.rs original/tenant_inherited.rs src/publication.rs src/helper.rs src/host-envelope.rs src/guest-envelope.rs src/envelope_tests.rs > "$task_root/format.log" 2>&1
cp original/tenant_exit.rs src/tenant_exit.rs
cp original/tenant_stdio.rs src/tenant_stdio.rs
cp original/tenant_inherited.rs src/tenant_inherited.rs
set +e
cargo check --tests --offline -j1 > "$task_root/check.log" 2>&1
check=$?
cargo clippy --tests --offline -j1 -- -D warnings > "$task_root/clippy.log" 2>&1
clippy=$?
cargo check --tests --offline -j1 --target x86_64-unknown-linux-musl > "$task_root/musl-check.log" 2>&1
musl_check=$?
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings > "$task_root/musl-clippy.log" 2>&1
musl_clippy=$?
cargo test --lib --offline -j1 envelope_tests:: -- --nocapture --test-threads=1 > "$task_root/pure-test.log" 2>&1
pure_test=$?
cargo test --lib --offline -j1 --target x86_64-unknown-linux-musl envelope_tests:: -- --nocapture --test-threads=1 > "$task_root/musl-pure-test.log" 2>&1
musl_pure_test=$?
python3 policy/scripts/check-development-policy > "$task_root/policy.log" 2>&1
policy=$?
set -e
for item in check clippy musl_check musl_clippy pure_test musl_pure_test policy; do eval "printf '%s\\n' \"\$$item\" > \"$task_root/$item.exit\""; done
du -sk "$task_root/source" "$task_root/target" > "$task_root/disk.txt"
python3 - <<'PY'
from pathlib import Path
import json,hashlib
r=Path.home()/'managed-tenant-envelope-light-01a1c5d2-v1'
(r/'checked-hashes.json').write_text(json.dumps({str(p.relative_to(r/'source')):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((r/'source').rglob('*')) if p.is_file()},indent=2)+'\n')
print(json.dumps({p.name:p.read_text() for p in r.glob('*.exit')}))
for n in ['check.log','clippy.log','musl-check.log','musl-clippy.log','pure-test.log','musl-pure-test.log','policy.log','disk.txt']:print(n+'\n'+(r/n).read_text())
PY
test "$check" = 0 && test "$clippy" = 0 && test "$musl_check" = 0 && test "$musl_clippy" = 0 && test "$pure_test" = 0 && test "$musl_pure_test" = 0 && test "$policy" = 0
