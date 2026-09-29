#!/usr/bin/env bash
# Builds the shards guest kernel in the pinned builder (resources/kernel/builder.env):
#   scripts/build-kernel-in-builder.sh x86_64|aarch64 OUT_DIR
# Output: what scripts/build-kernel.sh writes, plus OUT_DIR/toolchain-<arch>.txt, the exact
# compiler, linker and package versions. Needs Docker; the builder runs the host's
# architecture, which must be the kernel's.
set -euo pipefail

arch=${1:?usage: build-kernel-in-builder.sh x86_64|aarch64 OUT_DIR}
out=${2:?usage: build-kernel-in-builder.sh x86_64|aarch64 OUT_DIR}
repo=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=../resources/kernel/builder.env
. "$repo/resources/kernel/builder.env"
mkdir -p "$out"
out=$(cd "$out" && pwd)

docker run --rm -v "$repo:/src:ro" -v "$out:/out" \
    -e ARCH_="$arch" -e SNAPSHOT="$SNAPSHOT" -e PACKAGES="$PACKAGES" \
    "$BUILDER" bash -euo pipefail -c '
        rm -f /etc/apt/sources.list.d/*
        echo "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$SNAPSHOT bookworm main" \
            > /etc/apt/sources.list
        apt-get -o Acquire::Check-Valid-Until=false -o Acquire::Retries=5 update -q
        # shellcheck disable=SC2086
        apt-get -o Acquire::Retries=5 install -y -q --no-install-recommends $PACKAGES
        {
            gcc --version | head -1
            ld --version | head -1
            dpkg-query -W -f="\${Package} \${Version}\n" $PACKAGES gcc-12 binutils libc6-dev
        } > "/out/toolchain-$ARCH_.txt"
        /src/scripts/build-kernel.sh "$ARCH_" /out
        chown -R "'"$(id -u):$(id -g)"'" /out
    '
