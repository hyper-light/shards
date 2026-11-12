#!/bin/sh
# Builds $PKG's $BIN test binary for $TARGET and runs test $TEST $N times.
set -eu
export HOME=/home/shards CARGO_HOME=/cargo
cargo test --locked --profile test-release --target "$TARGET" -p "$PKG" --bin "$BIN" --no-run 2>&1 | tail -3
bin=$(cargo test --locked --profile test-release --target "$TARGET" -p "$PKG" --bin "$BIN" --no-run --message-format=json 2>/dev/null \
  | grep -o '"executable":"[^"]*"' | tail -1 | cut -d'"' -f4)
echo "binary: $bin"
pass=0
fail=0
i=0
while [ "$i" -lt "$N" ]; do
  i=$((i + 1))
  if out=$("$bin" --exact "$TEST" --test-threads 1 2>&1); then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "run $i FAILED:"
    echo "$out" | grep -E "panicked|os error|Custom|Error" | head -4
  fi
done
echo "RESULT test=$TEST n=$N pass=$pass fail=$fail"
