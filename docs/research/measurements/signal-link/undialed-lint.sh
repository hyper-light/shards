#!/bin/bash
# scripts/lint for all seven targets from the Mac Studio (kiln's adas-mac-studio.local-2), in
# a clone of its own at REV (~/to-guest/lint), with a target directory of its own.
#   undialed-lint.sh REV
set -u
rev=$1 base=$HOME/to-guest
export CARGO_TARGET_DIR=$base/target-lint
mkdir -p "$base/logs" "$base/tools"
cd "$base/lint" || exit 1
git checkout -q --detach "$rev" || exit 1
[ "$(zig version 2> /dev/null)" = 0.16.0 ] || PATH="$(TMPDIR=$base/tools scripts/install-zig):$PATH"
rustup target add --toolchain 1.98.1 x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
	x86_64-pc-windows-msvc aarch64-pc-windows-msvc > /dev/null 2>&1
log=$base/logs/lint.log out=$base/logs/lint.out
echo "# scripts/lint (all seven targets): $(git rev-parse --short=12 HEAD); $(date -u +%FT%TZ) $(uname -srm) macOS $(sw_vers -productVersion); $(rustc --version); $(cargo clippy --version); zig $(zig version)" > "$log"
nice -n 10 scripts/lint > "$out" 2>&1
rc=$?
grep -aE "^== |^(warning|error)" -A 8 "$out" | grep -vE "^\s+(Compiling|Checking)" | head -150 >> "$log"
echo "## scripts/lint exit $rc" >> "$log"
