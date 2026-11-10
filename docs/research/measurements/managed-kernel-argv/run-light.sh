#!/usr/bin/env bash
# Pure unit/compile/lint checks only; never boots a VM.
set -euo pipefail
: "${CARGO_TARGET_DIR:?absolute private target required}"
: "${RECORDS:?absolute private records directory required}"
case "$CARGO_TARGET_DIR:$RECORDS" in /*:/*) ;; *) exit 2 ;; esac
mkdir -p "$RECORDS" "$CARGO_TARGET_DIR"
{
    pwd
    git rev-parse HEAD
    git diff -- crates/vmm/src/vm/x86_64.rs crates/vmm/src/vm/mod.rs
    sha256sum crates/vmm/src/vm/{cmdline.rs,x86_64.rs,mod.rs}
    rustc -vV
    cargo -vV
    rustup target list --installed
    uname -a
    id
    df -k "$CARGO_TARGET_DIR/.."
} > "$RECORDS/identity.txt" 2>&1
# Kiln sync preserves mtimes: explicitly discard this crate's private artifacts
# and refresh owned inputs so an earlier instrumented artifact cannot be reused.
cargo clean -p shards-vmm > "$RECORDS/clean.txt" 2>&1
touch crates/vmm/src/vm/{cmdline.rs,x86_64.rs,mod.rs}
run() {
    local name="$1"
    shift
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
run gnu-unit cargo test -p shards-vmm --lib --target x86_64-unknown-linux-gnu -j4 vm::cmdline::tests -- --nocapture
run gnu-clippy cargo clippy -p shards-vmm --lib --tests --target x86_64-unknown-linux-gnu -j4 -- -D warnings
run musl-unit cargo test -p shards-vmm --lib --target x86_64-unknown-linux-musl -j4 vm::cmdline::tests -- --nocapture
run musl-clippy cargo clippy -p shards-vmm --lib --tests --target x86_64-unknown-linux-musl -j4 -- -D warnings
run windows-gnu-check cargo check -p shards-vmm --lib --target x86_64-pc-windows-gnu -j4
python3 - "$CARGO_TARGET_DIR" "$RECORDS" <<'PY'
import hashlib, json, pathlib, sys
target, records = map(pathlib.Path, sys.argv[1:])
artifacts = []
for triple in ('x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-musl'):
    for path in sorted((target / triple / 'debug/deps').glob('shards_vmm-*')):
        if path.is_file() and path.stat().st_mode & 0o111:
            artifacts.append(dict(path=str(path), bytes=path.stat().st_size,
                                  sha256=hashlib.sha256(path.read_bytes()).hexdigest()))
(records / 'unit-binaries.json').write_text(json.dumps(artifacts, indent=2) + '\n')
PY
