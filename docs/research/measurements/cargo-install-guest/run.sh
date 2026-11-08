#!/bin/sh
# What `cargo install` honors when a host binary embeds a guest binary built by its
# build script (a nested `cargo build` for <arch>-unknown-linux-musl, include_bytes!).
# Usage: run.sh WORKDIR   (needs rustup with toolchain 1.98.0 + both musl targets, and a
# default toolchain; the host binary prints what its build saw.)
set -eu
X=${1:?usage: run.sh WORKDIR}
here=$(cd "$(dirname "$0")" && pwd)
rm -rf "$X" && mkdir -p "$X" && cp -R "$here/repo" "$X/repo" && rm -rf "$X/repo/target"
cd "$X/repo" && git init -q . 2>/dev/null; git add -A && git -c user.name=exp -c user.email=exp@localhost commit -qm exp || true
echo "== A: cargo build, cwd in the repo"; cargo build -q; ./target/debug/exp-host
echo "== B: cargo install --path host, cwd in the repo"; cargo install -q --path host --root "$X/rootB" --offline; "$X/rootB/bin/exp-host"
cd "$X"
echo "== C: cargo install --path <repo>/host, cwd outside"; cargo install -q --path "$X/repo/host" --root "$X/rootC" --offline; "$X/rootC/bin/exp-host"
echo "== D: cargo install --git file://<repo>, cwd outside"; cargo install -q --git "file://$X/repo" exp-host --root "$X/rootD"; "$X/rootD/bin/exp-host"
echo "== E: as D, with RUSTUP_TOOLCHAIN=1.98.0"; RUSTUP_TOOLCHAIN=1.98.0 cargo install -q --git "file://$X/repo" exp-host --root "$X/rootE"; "$X/rootE/bin/exp-host"
echo "== F: as E, with the musl linker forced back to cc"; RUSTUP_TOOLCHAIN=1.98.0 CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo install -q --git "file://$X/repo" exp-host --root "$X/rootF"; "$X/rootF/bin/exp-host"
