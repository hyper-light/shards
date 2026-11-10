#!/usr/bin/env bash
# Coordinator-approved pure-helper test. Invoke under lockf /tmp/g121-mac.lock.
set -euo pipefail
: "${OUTPUT:?absolute private output directory required}"
: "${RECORDS:?absolute private records directory required}"
case "$OUTPUT:$RECORDS" in /*:/*) ;; *) exit 2 ;; esac
mkdir -p "$OUTPUT" "$RECORDS"
python3 - "$OUTPUT" "$RECORDS" <<'PY'
import hashlib, json, os, pathlib, re, sys
output, records = map(pathlib.Path, sys.argv[1:])
space = os.statvfs(output)
available = space.f_bavail * space.f_frsize
if available < 30 * 1024**3:
    raise SystemExit('held: Mac below required30GiB floor')
arch = pathlib.Path('crates/vmm/src/arch/x86_64/mod.rs')
boot = pathlib.Path('crates/vmm/src/arch/x86_64/boot.rs')
helper = pathlib.Path('crates/vmm/src/vm/cmdline.rs').resolve()
layout = '\n'.join(re.findall(r'^    pub const CMDLINE(?:_MAX)?:[^\n]+', arch.read_text(), re.M))
variant = re.search(r'^    DoesNotFit \{[^\n]+', boot.read_text(), re.M).group()
# Test adapter only: these exact existing declarations resolve the helper imports.
# This does not compile the real BootError, VMM crate, guest or target-specific ABI.
wrapper = ('mod arch { pub mod x86_64 { pub mod layout {\n' + layout +
           '\n} pub mod boot { #[derive(Debug)] pub enum BootError {\n' +
           variant + '\n} } } }\n#[path = ' + json.dumps(str(helper)) + '] mod cmdline;\n')
(output / 'wrapper.rs').write_text(wrapper)
identities = [dict(path=str(p), sha256=hashlib.sha256(p.read_bytes()).hexdigest())
              for p in (arch, boot, helper, output / 'wrapper.rs')]
(records / 'sources.json').write_text(json.dumps(dict(available_bytes=available, sources=identities), indent=2) + '\n')
PY
{
    git rev-parse HEAD
    rustc +1.98.1 -vV
    uname -a
    df -k "$OUTPUT"
} > "$RECORDS/identity.txt" 2>&1
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
run compile rustc +1.98.1 --edition 2024 --test -C codegen-units=4 "$OUTPUT/wrapper.rs" -o "$OUTPUT/cmdline-unit"
run unit "$OUTPUT/cmdline-unit" --nocapture --test-threads=4
shasum -a256 "$OUTPUT/wrapper.rs" "$OUTPUT/cmdline-unit" > "$RECORDS/artifacts.sha256"
