#!/usr/bin/env bash
# No sockets/namespaces/VMs are exercised: filtered pure input/encoding tests only.
set -euo pipefail
: "${CARGO_TARGET_DIR:?absolute private target required}"
: "${RECORDS:?absolute private records required}"
case "$CARGO_TARGET_DIR:$RECORDS" in /*:/*) ;; *) exit 2 ;; esac
mkdir -p "$CARGO_TARGET_DIR" "$RECORDS"
{
    pwd
    git rev-parse HEAD
    sha256sum crates/testguest/src/managed_network.rs crates/testguest/src/managed_network/intercept{.rs,/wire.rs} crates/shards/tests/managed_network.rs
    rustc -vV
    cargo -vV
    uname -a
    rustup target list --installed
    df -k "$CARGO_TARGET_DIR"
} > "$RECORDS/identity.txt" 2>&1
cargo clean -p shards-testguest > "$RECORDS/clean.txt" 2>&1
touch crates/testguest/src/managed_network.rs crates/testguest/src/managed_network/intercept{.rs,/wire.rs}
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
run gnu-pure cargo test -p shards-testguest --bin shards-testguest --target x86_64-unknown-linux-gnu -j4 managed_network::intercept:: -- --nocapture
run parent-parser cargo test -p shards-testguest --bin shards-testguest --target x86_64-unknown-linux-gnu -j4 managed_network::tests::interception_commands_keep_explicit_ip_families -- --exact --nocapture
run gnu-clippy cargo clippy -p shards-testguest --bin shards-testguest --tests --target x86_64-unknown-linux-gnu -j4 -- -D warnings
run musl-check cargo clippy -p shards-testguest --bin shards-testguest --tests --target x86_64-unknown-linux-musl -j4 -- -D warnings
python3 - "$CARGO_TARGET_DIR" "$RECORDS" <<'PY'
import hashlib,json,pathlib,sys
target,records=map(pathlib.Path,sys.argv[1:])
found=[]
for p in sorted((target/'x86_64-unknown-linux-gnu/debug/deps').glob('shards_testguest-*')):
    if p.is_file() and p.stat().st_mode & 0o111:
        found.append(dict(path=str(p),bytes=p.stat().st_size,sha256=hashlib.sha256(p.read_bytes()).hexdigest()))
(records/'binaries.json').write_text(json.dumps(found,indent=2)+'\n')
PY
