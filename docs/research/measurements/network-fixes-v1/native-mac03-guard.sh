#!/bin/bash
set -euo pipefail
source_dir="$HOME/g121-tmp/network-arm02-v1/source"
for attempt in 1 2 3 4 5 6; do
  git -C "$source_dir" fetch origin p/fix-network-03-macos-v1 && break
  if [ "$attempt" = 6 ]; then exit 1; fi
  sleep 30
done
git -C "$source_dir" checkout --detach 18e5e2aa85ae583ccbc9cbcbe75ecbc1b9e91eb2
cd "$source_dir"
export PATH="$HOME/.cargo/bin:/usr/local/bin:$PATH"
export CARGO_TARGET_DIR="$HOME/g121-tmp/network-dns03-macos-v1/target"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 SHARDS_HELPERS=skip
uname -srm
rustc --version
git rev-parse HEAD
trap 'git restore -- crates/net/src/lib.rs' EXIT
python3 - <<'PY'
from pathlib import Path
p = Path("crates/net/src/lib.rs")
source = p.read_text()
guard = "was.until > now && !(was.answer.name == answer.name && was.answer.kind == answer.kind)"
assert source.count(guard) == 1
p.write_text(source.replace(guard, "was.until > now"))
PY
set +e
cargo test -p shards-net --profile test-release --lib dns_answers_replace_the_names_previous_authority > "$HOME/g121-tmp/network-dns03-macos-v1/mutation.log" 2>&1
status=$?
set -e
cat "$HOME/g121-tmp/network-dns03-macos-v1/mutation.log"
git restore -- crates/net/src/lib.rs
trap - EXIT
if [ "$status" != 101 ] || ! grep -q 'the replaced DNS answer still grants a new flow' "$HOME/g121-tmp/network-dns03-macos-v1/mutation.log"; then
  exit 1
fi
bash docs/research/measurements/network-fixes-v1/native-mac03.sh green
