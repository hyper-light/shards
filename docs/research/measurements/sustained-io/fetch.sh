#!/bin/sh
# What the sustained-I/O benchmark runs (run.py), fetched and built into SIO (default
# ~/sio), as its user: each external pinned by version and checked by its digest or
# commit. Linux, x86_64 or aarch64; needs git, curl, node and npm (npm/cli v12.2.1's
# engines: ^22.22.2, ^24.15.0 or >= 26), cargo, zig 0.16.0 (ZIG, default zig on PATH;
# scripts/install-zig) and Docker, which builds virtiofsd in a pinned Alpine image.
# Usage: fetch.sh
set -eu
SIO=${SIO:-$HOME/sio}
ZIG=${ZIG:-zig}
repo=$(cd "$(dirname "$0")/../../../.." && pwd)
mkdir -p "$SIO/bin" "$SIO/src" "$SIO/dl"

# fetch URL SHA256 FILE: FILE, once its bytes are those.
fetch() {
    if [ -f "$3" ] && echo "$2  $3" | sha256sum -c --quiet 2>/dev/null; then return; fi
    curl -sSfL -o "$3.part" "$1"
    echo "$2  $3.part" | sha256sum -c --quiet
    mv "$3.part" "$3"
}

# checkout URL TAG COMMIT DIR: the tag's tree, which must be that commit.
checkout() {
    [ -d "$4/.git" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch "$2" "$1" "$4"
    at=$(git -C "$4" rev-parse HEAD)
    [ "$at" = "$3" ] || { echo "$1 $2 is $at, not $3" >&2; exit 1; }
}

case $(uname -m) in
x86_64)
    arch=x86_64
    fc=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
    ch=cloud-hypervisor-static
    chsum=4f4f98a3d661a40f3e5272627cc895a1b2e525f2f6bedf5f895f9cd21ba5c65b
    kernel=vmlinux-6.18.48-x86_64
    ksum=b47a9c50f1564d5af8a9e8d12c00324a94cd7a3d556f91651430398e8887dbac
    ;;
aarch64)
    arch=aarch64
    fc=e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256
    ch=cloud-hypervisor-static-aarch64
    chsum=e8a6d9786ea678e739f1a386682846e93742dcb9c7b6e3e0c62e3bda6cff0ef3
    kernel=Image-6.18.48-aarch64
    ksum=99b434f2cbe898eb3df4c6479cc4417d848b15c51050c7034004c0f0f00e19f1
    ;;
*) echo "$(uname -m): no Firecracker or Cloud Hypervisor release" >&2; exit 1 ;;
esac

# shards' pinned guest kernel (crates/shards/src/kernel.rs), which every VMM here boots.
grep -q "$ksum" "$repo/crates/shards/src/kernel.rs" ||
    { echo "kernel.rs no longer pins $kernel $ksum" >&2; exit 1; }
fetch "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/$kernel" \
    "$ksum" "$SIO/bin/kernel"

# Firecracker v1.17.0 and Cloud Hypervisor v54.0, their release binaries.
fetch "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-$arch.tgz" \
    "$fc" "$SIO/dl/firecracker.tgz"
tar -xzf "$SIO/dl/firecracker.tgz" -C "$SIO/dl"
cp "$SIO/dl/release-v1.17.0-$arch/firecracker-v1.17.0-$arch" "$SIO/bin/firecracker"
fetch "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v54.0/$ch" \
    "$chsum" "$SIO/bin/cloud-hypervisor"
chmod +x "$SIO/bin/firecracker" "$SIO/bin/cloud-hypervisor"

# fio 3.43, static for musl, the same binary in every guest. Its psync engine alone:
# Docker's seccomp profile, and so shards', refuse io_uring, and libaio is a library
# more; concurrency is numjobs.
checkout https://github.com/axboe/fio.git fio-3.43 d2dcd7e053d1902d2420f151c83990e6acdd9f8c "$SIO/src/fio"
if [ ! -x "$SIO/bin/fio" ]; then
    (
        cd "$SIO/src/fio"
        export ZIG_TARGET="$arch-linux-musl" PATH="$(dirname "$(command -v "$ZIG")"):$PATH"
        ./configure --cc="$repo/scripts/zig-cc" --build-static --disable-native >"$SIO/dl/fio-configure.txt"
        make -s -j"$(nproc)" fio
    )
    cp "$SIO/src/fio/fio" "$SIO/bin/fio"
fi

# virtiofsd v1.14.0, Cloud Hypervisor's virtio-fs backend, static for musl as its own CI
# builds it (.gitlab-ci.yml: the target named, so that the static flags pass its
# proc-macros by), in rust:1-alpine3.22; the image is removed again if it was not here
# before.
checkout https://gitlab.com/virtio-fs/virtiofsd.git v1.14.0 c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd "$SIO/src/virtiofsd"
if [ ! -x "$SIO/bin/virtiofsd" ]; then
    rust=mirror.gcr.io/library/rust@sha256:2b3b5cdd9112f19366d21b94541ffcd3d58bf838f696451a0d50c8032088cb15
    had=$(docker image inspect "$rust" >/dev/null 2>&1 && echo yes || echo no)
    docker run --rm --name "sio-virtiofsd-$$" -v "$SIO/src/virtiofsd:/src:ro" "$rust" sh -c '
        apk add -q libseccomp-dev libseccomp-static libcap-ng-dev libcap-ng-static >&2 &&
        cp -r /src /tmp/b && cd /tmp/b &&
        RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=yes" \
        LIBSECCOMP_LINK_TYPE=static LIBSECCOMP_LIB_PATH=/usr/lib \
        LIBCAPNG_LINK_TYPE=static LIBCAPNG_LIB_PATH=/usr/lib \
        cargo build -q --locked --release --target "$(uname -m)-unknown-linux-musl" >&2 &&
        cat target/*-unknown-linux-musl/release/virtiofsd' > "$SIO/bin/virtiofsd.part"
    [ "$had" = yes ] || docker image rm -f "$rust" >/dev/null
    chmod +x "$SIO/bin/virtiofsd.part"
    mv "$SIO/bin/virtiofsd.part" "$SIO/bin/virtiofsd"
fi

# npm/cli v12.2.1, whose lockfile (v3) lists 1,202 packages: installed once here to fill
# a cache that the guests' `npm ci --offline` installs from, then its node_modules gone.
checkout https://github.com/npm/cli.git v12.2.1 ae98c62586b6fea44114f72e249e246a2981de70 "$SIO/src/cli"
if [ ! -d "$SIO/npm-cache/_cacache" ]; then
    (cd "$SIO/src/cli" && npm ci -q --cache "$SIO/npm-cache" --ignore-scripts --no-audit --no-fund)
    find "$SIO/src/cli" -name node_modules -type d -prune -exec rm -rf {} +
fi

# rust-analyzer 2026-10-19 (377 packages in its lockfile, 332 vendored) with its
# dependencies vendored, which `cargo build --offline` builds and `cargo test` tests.
checkout https://github.com/rust-lang/rust-analyzer.git 2026-10-19 5ec20d044dbf04895877a55921ddc0ae6e73e127 \
    "$SIO/src/rust-analyzer"
if [ ! -d "$SIO/src/rust-analyzer/vendor" ]; then
    mkdir -p "$SIO/src/rust-analyzer/.cargo"
    # Not -q: that leaves out the configuration it prints, which builds read the copies by.
    (cd "$SIO/src/rust-analyzer" && cargo vendor --locked vendor >.cargo/config.toml)
    grep -q vendored-sources "$SIO/src/rust-analyzer/.cargo/config.toml"
fi

"$SIO/bin/firecracker" --version | head -n 1
"$SIO/bin/cloud-hypervisor" --version
"$SIO/bin/fio" --version
"$SIO/bin/virtiofsd" --version
sha256sum "$SIO/bin/"*
