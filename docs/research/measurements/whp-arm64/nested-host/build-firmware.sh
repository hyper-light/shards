#!/bin/bash
# M414's TF-A + UEFI stack, built on macOS. KIT contains tfa-src and edk2-src at
# README.md's revisions. Requires Homebrew llvm, lld, make, gnu-sed, acpica, openssl@3,
# and an edk2-tools Python venv with antlr4-python3-runtime==4.13.2, regex and pefile.
set -eu
kit=$(cd "${1:?usage: build-firmware.sh KIT}" && pwd)
export PATH="$(brew --prefix make)/libexec/gnubin:$(brew --prefix gnu-sed)/libexec/gnubin:$(brew --prefix llvm)/bin:/opt/homebrew/bin:/usr/bin:/bin"
export PYTHON_COMMAND="$kit/edk2-tools/bin/python"
export CLANGDWARF_BIN="$(brew --prefix llvm)/bin/"
export CLANG_HOST_BIN="$(brew --prefix make)/libexec/gnubin/"
(
    cd "$kit/edk2-src"
    git submodule update --init --depth=1
    make -C BaseTools -j8
    # edksetup reads optional unset variables.
    set +u
    source edksetup.sh
    set -u
    build -a AARCH64 -t CLANGDWARF -p ArmVirtPkg/ArmVirtQemuKernel.dsc -b DEBUG -n8
)
gmake -C "$kit/tfa-src" PLAT=qemu DEBUG=1 \
    CC="$(brew --prefix llvm)/bin/clang" LD="$(brew --prefix lld)/bin/ld.lld" clean
gmake -C "$kit/tfa-src" -j8 PLAT=qemu DEBUG=1 QEMU_USE_GIC_DRIVER=QEMU_GICV3 \
    CC="$(brew --prefix llvm)/bin/clang" LD="$(brew --prefix lld)/bin/ld.lld" \
    HOSTCC=/usr/bin/clang OPENSSL_DIR="$(brew --prefix openssl@3)" \
    BL33="$kit/edk2-src/Build/ArmVirtQemuKernel-AArch64/DEBUG_CLANGDWARF/FV/QEMU_EFI.fd" all fip
mkdir -p "$kit/firmware"
cp "$kit/tfa-src/build/qemu/debug/qemu_fw.rom" "$kit/firmware/tfa-qemu.rom"
