#!/bin/sh
# CI's native glibc job on this architecture, approximated: Debian's rust image, seccomp off
# as on a runner, the tests as the checkout's owner, never root. Runs TEST_ARGS for $TARGET,
# then the code-mode helper stopped and continued (stop-helper.sh, workers too).
set -eu
# What rust-toolchain.toml asks for, installed as root before the tests run as the owner.
cd /work && rustup show active-toolchain >/dev/null
uid=$(stat -c %u /work)
useradd -m -u "$uid" -d /home/shards shards
exec su shards -s /bin/sh -c "
  set -eu
  export HOME=/home/shards CARGO_HOME=/cargo PATH=/usr/local/cargo/bin:\$PATH RUSTUP_HOME=/usr/local/rustup
  cd /work
  cargo test --locked --profile test-release --target $TARGET $TEST_ARGS
  b=\$(ls -t target/$TARGET/test-release/deps/shards_server-* | grep -v '\.d\$' | head -1)
  WORKERS=1 sh /cigbin/stop-helper.sh \"\$b\" 10
"
