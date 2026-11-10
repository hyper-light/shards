#!/bin/sh
# Compile the Linux-only probe and its parser tests without rebuilding testguest's TLS.
set -eu
test "$(uname -s)" = Linux
TASK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/shards-managed-network-unit.XXXXXX")
trap 'rm -rf "$TASK_DIR"' EXIT
LIBC_VERSION=$(awk '$0 == "name = \"libc\"" { found=1; next } found && $1 == "version" { gsub(/"/, "", $3); print $3; exit }' Cargo.lock)
test -n "$LIBC_VERSION"
cp crates/testguest/src/managed_network.rs "$TASK_DIR/probe.rs"
cp clippy.toml "$TASK_DIR/clippy.toml"
cat > "$TASK_DIR/Cargo.toml" <<EOF
[package]
name = "managed-network-probe-light-check"
version = "0.0.0"
edition = "2024"
[dependencies]
libc = "=$LIBC_VERSION"
[[bin]]
name = "probe"
path = "main.rs"
EOF
cat > "$TASK_DIR/main.rs" <<'EOF'
#[path = "probe.rs"]
mod probe;
fn main() { std::process::exit(probe::main()); }
EOF
git rev-parse HEAD
sha256sum crates/testguest/src/managed_network.rs
uname -a
rustc --version
cargo test --manifest-path "$TASK_DIR/Cargo.toml" -j4
cargo clippy --manifest-path "$TASK_DIR/Cargo.toml" --all-targets -j4 -- -D warnings \
    -D clippy::panic -D clippy::unwrap_used -D clippy::expect_used \
    -D clippy::indexing_slicing -D clippy::undocumented_unsafe_blocks
