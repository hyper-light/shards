#!/usr/bin/env bash
# Builds the shards guest kernel for one architecture, reproducibly:
#   scripts/build-kernel.sh x86_64|aarch64 OUT_DIR
# Output: OUT_DIR/vmlinux-<version>-x86_64 (ELF) or OUT_DIR/Image-<version>-aarch64, and
# OUT_DIR/config-<version>-<arch>. Needs a Linux host with gcc, make, bc, bison, flex,
# patch, libelf and libssl headers; scripts/build-kernel-in-builder.sh runs it in the
# pinned builder, which releases use. resources/kernel/README.md explains the inputs.
set -euo pipefail

arch=${1:?usage: build-kernel.sh x86_64|aarch64 OUT_DIR}
out=${2:?usage: build-kernel.sh x86_64|aarch64 OUT_DIR}

version=6.18.48
# From kernel.org's sha256sums.asc, signed by the Kernel.org checksum autosigner
# (key B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1).
tarball_sha256=5ebdadb10a4b5708fc6b1c457764a110bc49f8150cc3502c59b921ead8c6fc8c

case $arch in
x86_64) karch=x86 target=vmlinux built=vmlinux name=vmlinux ;;
aarch64) karch=arm64 target=Image built=arch/arm64/boot/Image name=Image ;;
*)
    echo "unknown architecture $arch" >&2
    exit 2
    ;;
esac
if [ "$(uname -m)" != "$arch" ]; then
    echo "build $arch kernels on $arch hosts: guests run the host's architecture" >&2
    exit 2
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL --retry 3 -o "$work/linux.tar.xz" \
    "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$version.tar.xz"
echo "$tarball_sha256  $work/linux.tar.xz" | sha256sum -c --quiet -
tar -xJf "$work/linux.tar.xz" -C "$work"
src=$work/linux-$version

# shards' fixes to the kernel, in order (resources/kernel/patches). Each must apply
# exactly, with no fuzz.
for p in "$repo"/resources/kernel/patches/*.patch; do
    [ -e "$p" ] || continue
    patch -d "$src" -p1 --forward --batch --fuzz=0 --no-backup-if-mismatch <"$p"
done

# Ours for every architecture, then the architecture's own, if it has one. No option is
# named by both: each says one thing, and says why where it is.
fragments=("$repo/resources/kernel/shards.config")
if [ -e "$repo/resources/kernel/shards-$arch.config" ]; then
    fragments+=("$repo/resources/kernel/shards-$arch.config")
fi
named=$(sed -nE 's/^(CONFIG_[A-Za-z0-9_]+)=.*/\1/p; s/^# (CONFIG_[A-Za-z0-9_]+) is not set$/\1/p' "${fragments[@]}" | sort | uniq -d)
if [ -n "$named" ]; then
    echo "named by more than one fragment: $named" >&2
    exit 1
fi
cp "$repo/resources/kernel/firecracker-$arch-6.18.config" "$src/.config"
(cd "$src" && ARCH=$karch scripts/kconfig/merge_config.sh -m .config "${fragments[@]}" >/dev/null)
make -C "$src" ARCH=$karch olddefconfig >/dev/null

# Every option the fragments ask for must be in the final config, exactly.
missing=0
while IFS= read -r line; do
    case $line in
    CONFIG_*=*) grep -qxF -- "$line" "$src/.config" || { echo "not in .config: $line" >&2; missing=1; } ;;
    "# CONFIG_"*" is not set") grep -qxF -- "$line" "$src/.config" || { echo "not in .config: $line" >&2; missing=1; } ;;
    esac
done < <(cat "${fragments[@]}")
[ $missing -eq 0 ]

# Fixed build metadata, so the same inputs produce the same bytes.
export KBUILD_BUILD_TIMESTAMP='1970-01-01T00:00:00Z'
export KBUILD_BUILD_USER=shards KBUILD_BUILD_HOST=shards KBUILD_BUILD_VERSION=1
make -C "$src" ARCH=$karch -j"$(nproc)" "$target"

cp "$src/$built" "$out/$name-$version-$arch"
cp "$src/.config" "$out/config-$version-$arch"
(cd "$out" && sha256sum "$name-$version-$arch" "config-$version-$arch")
