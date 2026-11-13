# Build test binaries: build2.sh SRC TARGET TEST...
set -u
export PATH="$HOME/tools/zig-x86_64-windows-0.16.0:$HOME/tools/nasm-3.02:/c/Users/corph/.cargo/bin:/usr/bin:$PATH"
src=$1; export CARGO_TARGET_DIR=$2; shift 2
cd "$src" || exit 1
tests=""
for t in "$@"; do tests="$tests --test $t"; done
start=$(date +%s)
cargo test --locked --profile test-release -p shards $tests --no-run 2>&1 | grep -E "^error|Executable" | tail -8
status=${PIPESTATUS[0]}
echo "build $src exit $status after $(( $(date +%s) - start ))s"
exit $status
