#!/bin/bash
# scripts/lint on every supported target from a Mac (Linux through zig, Windows through
# cargo-xwin), in unit.sh's clone at its checkout: usage
#   bash lint.sh WORK [TARGET...]
set -u
work=${1:?work}; shift
src=$work/src
cd "$src" || exit 1
git log -1 --format='HEAD %H'
test -z "$(git status --porcelain)" || { echo "dirty tree"; git status --porcelain; exit 1; }
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
if ! command -v zig > /dev/null || [ "$(zig version)" != 0.16.0 ]; then
  mkdir -p "$work/zig"
  zbin=$(TMPDIR=$work/zig sh scripts/install-zig 2>"$work/logs/install-zig.log") || { cat "$work/logs/install-zig.log"; exit 1; }
  export PATH="$zbin:$PATH"
fi
echo "zig $(zig version); $(rustc --version); $(cargo xwin --version 2>/dev/null | head -1)"
export CARGO_TARGET_DIR=$work/lint-target
t0=$(date +%s)
sh scripts/lint "$@" > "$work/logs/lint.log" 2>&1
rc=$?
echo "LINT-EXIT $rc in $(( $(date +%s) - t0 )) s"
grep -a -E '^== |^(warning|error)(\[|:)|Finished' "$work/logs/lint.log" | sort | uniq -c | head -40
