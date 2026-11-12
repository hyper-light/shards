#!/bin/sh
# CI's native glibc job on this architecture, approximated (ci-green-arm64-v1's gnu-run.sh):
# Debian's rust image, what rust-toolchain.toml asks for installed as root, the checks as the
# checkout's owner, never root. Runs checks.sh from /work for $TARGET.
#
#   docker run --rm --security-opt seccomp=unconfined -v "$PWD:/work" -w /work -e TARGET \
#     IMAGE sh docs/research/measurements/musl-opath-v1/harness/container-gnu.sh
set -eu
cd /work && rustup show active-toolchain >/dev/null
uid=$(stat -c %u /work)
[ "$uid" != 0 ] || { echo "/work is root's; the checks run as its owner" >&2; exit 1; }
useradd -m -u "$uid" -d /home/shards shards
exec su shards -s /bin/sh -c "
  export HOME=/home/shards CARGO_HOME=/home/shards/.cargo PATH=/usr/local/cargo/bin:\$PATH RUSTUP_HOME=/usr/local/rustup TARGET=$TARGET
  exec sh docs/research/measurements/musl-opath-v1/harness/checks.sh
"
