#!/bin/bash
# ROOT (Hex lead; Shards coordinator for the 2026-11-15 vendor point, Ada's word): one pass of
# the vendor point's checks on one tree of Shards, in its own folder and target.
# Usage: svp.sh PATCH NAME STEPS...
#   The tree: tip-<sha>.tar.gz beside this script (git archive of the integration tip), unpacked
#   once; PATCH (none, r1.patch, m1.patch beside it) applied for this pass and taken off after.
#   NAME: the log folder's name; STEPS: any of clippy init unit e2e boot policy only:<test>:<filter>
#   watch:<secs>:<test>:<filter> (Linux: the test held to a deadline)
# Each step's exit code goes to $LOG/summary; the pass's exit is 0 only if every step's is.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
PATCH=$1 NAME=$2; shift 2
W=$HOME/.cache/hex-lead/svp; LOG=$W/logs/$NAME
mkdir -p "$LOG" "$W/tmp"
export CARGO_TARGET_DIR=$W/target TMPDIR=$W/tmp PATH=$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH
OS=$(uname -s) ARCH=$(uname -m); [ "$ARCH" = arm64 ] && ARCH=aarch64
MUSL=$ARCH-unknown-linux-musl
say() { echo "=== $(date -u +%H:%M:%SZ) $*" | tee -a "$LOG/summary"; }
TAR=$(ls "$HERE"/tip-*.tar.gz | head -1); TIP=${TAR##*/tip-}; TIP=${TIP%.tar.gz}
SRC=$W/src-$TIP
if [ ! -f "$SRC/.svp-ready" ]; then
  rm -rf "$SRC" && mkdir -p "$SRC" && tar -xzf "$TAR" -C "$SRC" && (cd "$SRC" && git init -q && git add -A) && touch "$SRC/.svp-ready" || { say "unpack failed"; exit 1; }
fi
cd "$SRC"
git checkout -q -- . || { say "reset to the tip failed"; exit 1; }
if [ "$PATCH" != none ]; then git apply "$HERE/$PATCH" || { say "apply $PATCH failed"; exit 1; }; fi
trap 'cd "$SRC" && git checkout -q -- .' EXIT
say "pass $NAME: tip $TIP + $PATCH on $(hostname) $OS/$ARCH; steps: $*"
ZIGDIR=$(scripts/install-zig 2>>"$LOG/zig.log" | tail -1); [ -d "$ZIGDIR" ] && export PATH=$ZIGDIR:$PATH
fail=0
run() { local step=$1; shift; local t0=$(date +%s); "$@" > "$LOG/$step.log" 2>&1; local rc=$?
  say "$step exit $rc ($(( $(date +%s) - t0 )) s): $*"; [ $rc -eq 0 ] || fail=1; }
musl_env() { local v=$(echo $MUSL | tr - _); export "CC_$v=$SRC/scripts/zig-cc" "AR_$v=$SRC/scripts/zig-ar" ZIG_TARGET=$ARCH-linux-musl; }
for step in "$@"; do case $step in
  clippy) run clippy nice -n 10 cargo clippy -p shards-vmm -p shards --all-targets -- -D warnings ;;
  init) ( musl_env; run init-clippy nice -n 10 cargo clippy -p shards-init -p shards-testguest --all-targets --target $MUSL -- -D warnings
          [ "$OS" = Linux ] && run init-test nice -n 10 cargo test -p shards-init --target $MUSL; exit $fail ) || fail=1 ;;
  unit) if [ "$OS" = Darwin ]; then run unit-hvf nice -n 10 cargo test --profile test-release -p shards-vmm --lib -- hv::hvf::tests; fi
        run unit-api-run nice -n 10 cargo test --profile test-release -p shards --bin shards -- api_run::tests::an_orchestrators_run_may_plug_its_most_less_its_boot ;;
  e2e) run e2e env SHARDS_REQUIRE_VMS=1 nice -n 10 cargo test --profile test-release -p shards --no-fail-fast \
         --test api_image --test nesting --test member_exec --test managed_output --test tty --test engine_ring --test guest_time ;;
  boot) run boot env SHARDS_REQUIRE_VMS=1 nice -n 10 cargo test --profile test-release -p shards --test boot shards_api_ ;;
  policy) run policy python3 scripts/check-development-policy ;;
  only:*) t=${step#only:}; test=${t%%:*}; filt=${t#*:}
     run "only-$test" env SHARDS_REQUIRE_VMS=1 nice -n 10 cargo test --profile test-release -p shards --test "$test" -- $filt --nocapture ;;
  watch:*) t=${step#watch:}; secs=${t%%:*}; t=${t#*:}; test=${t%%:*}; filt=${t#*:}
     # Built first, outside the deadline; the run held to SECS seconds (GNU timeout ends its process group;
     # the unit's cgroup ends what is left as the unit ends). Exit 124 is the deadline.
     run "build-$test" env SHARDS_REQUIRE_VMS=1 nice -n 10 cargo test --profile test-release -p shards --test "$test" --no-run
     run "watch-$test" env SHARDS_REQUIRE_VMS=1 timeout -k 30 "$secs" nice -n 10 cargo test --profile test-release -p shards --test "$test" -- $filt --nocapture ;;
  *) say "unknown step $step"; fail=1 ;;
esac; done
say "pass $NAME end: $([ $fail -eq 0 ] && echo GREEN || echo RED)"
grep -h "^test result\|FAILED\|panicked at" "$LOG"/*.log 2>/dev/null | sort | uniq -c | sort -rn | head -40 > "$LOG/digest"
exit $fail
