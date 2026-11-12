#!/bin/sh
# Item 5 (the restore bench's cause) on Linux, with no VM: the shards binary's `restore DIR --no-console`, fixed, then
# with console_said's options put after `restore` as da8bcd272 put them (the mutant), which
# cli::said_options must fail; the source put back and the binary built again after.
set -eu
export HOME=/home/shards CARGO_HOME=/cargo
f=crates/shards/src/cli/mod.rs
bin=$PWD/target/$TARGET/test-release/shards
b() { cargo build --locked --profile test-release --target "$TARGET" -p shards --bin shards 2>&1 | tail -1; }
b
echo "### fixed"
sh /cigbin/restore-args.sh "$bin" /tmp/ra-fixed
cp "$f" /tmp/kept-mod.rs
sed -i 's#^    with.extend(args.iter().cloned());$#    with.extend(args.first().cloned());#' "$f"
sed -i 's#^    (with, recording)$#    { with.extend(args.iter().skip(1).cloned()); (with, recording) }#' "$f"
if cmp -s "$f" /tmp/kept-mod.rs; then echo "MUTANT not applied"; exit 1; fi
b
echo "### mutant: the options after restore, as da8bcd272 put them"
sh /cigbin/restore-args.sh "$bin" /tmp/ra-mutant
cargo test --locked --profile test-release --target "$TARGET" -p shards --bin shards -- cli::said_options 2>&1 \
  | grep -E "^test |test result|panicked|left|right" | head -8
cp /tmp/kept-mod.rs "$f"
touch "$f"
b
cargo test --locked --profile test-release --target "$TARGET" -p shards --bin shards --no-run 2>&1 | tail -1
cmp "$f" /tmp/kept-mod.rs && echo "source put back"
