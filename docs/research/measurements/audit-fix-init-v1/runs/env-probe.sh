#!/usr/bin/env bash
# Read-only facts of the runner used for this lane's light checks: kernel, distribution,
# toolchains and targets already installed (RUSTUP_AUTO_INSTALL=0: nothing is installed),
# and whether strace exists. Run from the synced worktree root.
set -u
echo "== uname"; uname -srmo
echo "== os-release"; grep -E '^(NAME|ID|VERSION_ID)=' /etc/os-release
echo "== rustup"; rustup --version 2>&1 | head -2
echo "== toolchains"; rustup toolchain list
echo "== active (repo pin)"; RUSTUP_AUTO_INSTALL=0 rustup show active-toolchain 2>&1
echo "== targets of 1.98.1"; RUSTUP_AUTO_INSTALL=0 rustup target list --installed --toolchain 1.98.1 2>&1
echo "== components of 1.98.1"; RUSTUP_AUTO_INSTALL=0 rustup component list --installed --toolchain 1.98.1 2>&1
echo "== rustc"; RUSTUP_AUTO_INSTALL=0 rustc -Vv 2>&1
echo "== cargo"; RUSTUP_AUTO_INSTALL=0 cargo -V 2>&1
echo "== strace"; command -v strace && strace -V 2>&1 | head -1
echo "== nproc/mem"; nproc; grep -E 'MemTotal|MemAvailable' /proc/meminfo
echo "== disk of cwd"; df -h . | tail -1
echo "== cwd"; pwd; git rev-parse HEAD; git status --short | head -20
