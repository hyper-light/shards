#!/usr/bin/env bash
# IMPLEMENTER LIGHT ONLY: compile actual fixture modules, run filtered pure tests.
# No guest boot, sockets, namespaces, C dependencies, or full workspace build.
set -euo pipefail
: "${LIGHT_ROOT:?new absolute private helper source directory}"
: "${CARGO_TARGET_DIR:?absolute private target}"
: "${RECORDS:?new absolute private evidence directory}"
case "$LIGHT_ROOT:$CARGO_TARGET_DIR:$RECORDS" in /*:/*:/*) ;; *) exit 2 ;; esac
mkdir "$LIGHT_ROOT" "$RECORDS"
mkdir -p "$LIGHT_ROOT/crates/testguest/src" "$LIGHT_ROOT/docs/research/measurements/managed-network-intercept"
cp docs/research/measurements/managed-network-intercept/ipv4-wire-before.hex "$LIGHT_ROOT/docs/research/measurements/managed-network-intercept/"
cp crates/testguest/src/managed_network.rs "$LIGHT_ROOT/crates/testguest/src/"
cp -R crates/testguest/src/managed_network "$LIGHT_ROOT/crates/testguest/src/"
cp clippy.toml "$LIGHT_ROOT/"
cat > "$LIGHT_ROOT/Cargo.toml" <<'TOML'
[package]
name = "managed-network-udp-check"
version = "0.0.0"
edition = "2024"
[dependencies]
libc = "=0.2.189"
[[bin]]
name = "probe"
path = "main.rs"
[lints.rust]
unsafe_op_in_unsafe_fn = "deny"
[lints.clippy]
panic = "deny"
unwrap_used = "deny"
expect_used = "deny"
indexing_slicing = "deny"
TOML
cat > "$LIGHT_ROOT/main.rs" <<'RUST'
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]
#[path = "crates/testguest/src/main-wrapper.rs"]
mod native;
fn main() { std::process::exit(native::managed_network::main()); }
RUST
printf 'pub mod managed_network;\n' > "$LIGHT_ROOT/crates/testguest/src/main-wrapper.rs"
{
    git rev-parse HEAD
    git status --short
    rustc +1.98.1 -vV
    cargo +1.98.1 -vV
    rustup target list --installed --toolchain 1.98.1
    uname -a
    df -k "$CARGO_TARGET_DIR"
    sha256sum crates/testguest/src/managed_network{.rs,/intercept.rs,/intercept/wire.rs,/intercept/udp.rs} crates/shards/tests/managed_network.rs
} > "$RECORDS/identity.txt" 2>&1
run() {
    local name="$1"; shift
    printf '%q ' "$@" > "$RECORDS/$name.command"
    printf '\n' >> "$RECORDS/$name.command"
    set +e
    "$@" > "$RECORDS/$name.log" 2>&1
    local status=$?
    set -e
    printf '%s\n' "$status" > "$RECORDS/$name.exit"
    cat "$RECORDS/$name.log"
    return "$status"
}
# New copied source plus a NEW target force freshness; no other cache is written.
run gnu-pure cargo +1.98.1 test --offline --manifest-path "$LIGHT_ROOT/Cargo.toml" --target x86_64-unknown-linux-gnu -j4 native::managed_network::intercept:: -- --nocapture
run udp-parser cargo +1.98.1 test --offline --manifest-path "$LIGHT_ROOT/Cargo.toml" --target x86_64-unknown-linux-gnu -j4 native::managed_network::tests::udp_interception_parser_has_explicit_origins -- --exact --nocapture
run tcp-parser cargo +1.98.1 test --offline --manifest-path "$LIGHT_ROOT/Cargo.toml" --target x86_64-unknown-linux-gnu -j4 native::managed_network::tests::interception_commands_keep_explicit_ip_families -- --exact --nocapture
run gnu-clippy cargo +1.98.1 clippy --offline --manifest-path "$LIGHT_ROOT/Cargo.toml" --tests --target x86_64-unknown-linux-gnu -j4 -- -D warnings
run musl-clippy cargo +1.98.1 clippy --offline --manifest-path "$LIGHT_ROOT/Cargo.toml" --tests --target x86_64-unknown-linux-musl -j4 -- -D warnings
sha256sum "$CARGO_TARGET_DIR"/x86_64-unknown-linux-gnu/debug/deps/probe-* > "$RECORDS/artifact-sha256.txt"
cp "$LIGHT_ROOT/Cargo.lock" "$RECORDS/Cargo.lock"
