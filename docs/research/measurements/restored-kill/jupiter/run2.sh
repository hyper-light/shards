# Run a test binary: run2.sh SRC TARGET MASK BINARY [ARGS...]; MASK "-" for all CPUs.
set -u
export PATH="$HOME/tools/zig-x86_64-windows-0.16.0:$HOME/tools/nasm-3.02:/c/Users/corph/.cargo/bin:/usr/bin:$PATH"
export SHARDS_REQUIRE_VMS=1
src=$1; export CARGO_TARGET_DIR=$2; mask=$3; shift 3
cd "$src/crates/shards" || exit 1
bin=$(ls -t "$CARGO_TARGET_DIR"/test-release/deps/$1-*.exe | head -1)
shift
start=$(date +%s)
if [ "$mask" = "-" ]; then
  "$bin" "$@" 2>&1
else
  MSYS2_ARG_CONV_EXCL='*' cmd.exe /c "start /affinity $mask /wait /b $(cygpath -w "$bin") $*" 2>&1
fi
status=$?
echo "test exit $status after $(( $(date +%s) - start ))s"
exit $status
