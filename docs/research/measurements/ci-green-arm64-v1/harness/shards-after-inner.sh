#!/bin/sh
# The shards binary's unit tests after the fix: the CLI's (cli::said_options,
# cli::vm_flags) once, then the shippers' (daemon::ship::) $N times beside $BURN busy loops
# (loop-loaded.sh), as before the fix.
set -eu
export HOME=/home/shards CARGO_HOME=/cargo
cargo test --locked --profile test-release --target "$TARGET" -p shards --bin shards --no-run 2>&1 | tail -2
bin=$(cargo test --locked --profile test-release --target "$TARGET" -p shards --bin shards --no-run --message-format=json 2>/dev/null \
  | grep -o '"executable":"[^"]*"' | tail -1 | cut -d'"' -f4)
echo "binary: $bin"
"$bin" cli::said_options cli::vm_flags 2>&1 | grep -E "^test |test result"
BIN_PATH=$bin FILTER=daemon::ship:: sh /cigbin/loop-loaded.sh
