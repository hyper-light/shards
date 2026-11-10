set -eu
export CARGO_TARGET_DIR="$HOME/managed-borrowed-target"
evidence_root="$HOME/managed-borrowed-evidence"
evidence="$evidence_root/linux-eaedd0b0"
mkdir -p "$evidence"
git rev-parse HEAD > "$evidence/source.txt"
test "$(cat "$evidence/source.txt")" = eaedd0b0fe39662b9216a286d83e0e984c3cf2d3
git status --short > "$evidence/status.txt"
git diff --exit-code -- crates/api crates/afunix Cargo.lock
uname -a > "$evidence/uname.txt"
rustc -Vv > "$evidence/rustc.txt"
cargo -Vv > "$evidence/cargo.txt"
rustup target list --installed > "$evidence/installed-targets.txt"
sha256sum crates/api/src/*.rs crates/afunix/src/lib.rs Cargo.lock > "$evidence/source.sha256"
tar --exclude='*.tgz' --exclude='./linux-eaedd0b0' -czf "$evidence_root/owed-native-raw.tgz" -C "$evidence_root" .
touch crates/api/src/*.rs crates/afunix/src/lib.rs docs/research/measurements/managed-borrowed/*.rs
failed=0
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
  if test "$result" -ne 0; then failed=1; fi
}
for libc in gnu musl; do
  target="x86_64-unknown-linux-$libc"
  if ! test -d "$(rustc --print sysroot)/lib/rustlib/$target/lib"; then
    printf '%s\n' "$target standard library not installed; no install attempted" > "$evidence/$libc-unrun.txt"
    continue
  fi
  run_check "$libc-build" cargo test --locked -j4 --profile test-release --target "$target" -p shards-api -p shards-afunix --no-run --message-format=json-render-diagnostics
  if test "$(cat "$evidence/$libc-build.exit")" -eq 0; then
    python3 - "$evidence/$libc-build.log" "$evidence/$libc-binaries.json" <<'PY'
import hashlib,json,sys
from pathlib import Path
rows=[]
for line in Path(sys.argv[1]).read_text().splitlines():
    if not line.startswith("{"):continue
    row=json.loads(line)
    if row.get("reason")=="compiler-artifact" and row.get("executable"):
        p=Path(row["executable"])
        rows.append({"target":row["target"]["name"],"path":str(p),"sha256":hashlib.sha256(p.read_bytes()).hexdigest(),"fresh":row.get("fresh")})
Path(sys.argv[2]).write_text(json.dumps(rows,indent=2)+"\n")
PY
    run_check "$libc-corpus" cargo test --locked -j4 --profile test-release --target "$target" -p shards-api -p shards-afunix -- --test-threads=1 --nocapture
  fi
  run_check "$libc-clippy" cargo clippy --locked -j4 --target "$target" -p shards-api -p shards-afunix --all-targets -- -D warnings
done
git diff --exit-code -- crates/api crates/afunix Cargo.lock > "$evidence/restored-diff.txt"
printf '%s\n' "$failed" > "$evidence/summary.exit"
tar -czf "$evidence_root/linux-eaedd0b0-raw.tgz" -C "$evidence" .
exit "$failed"
