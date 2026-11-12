#!/bin/bash
# s38-api-remote.sh STAGE [ARGS]: D234 step 6's light checks (lane p/s38-api-v1), on a Linux
# host (adacachy). stdin: a `git archive` of the revision to check. It is unpacked into
# ~/s38-api-step6/in, then copied by content over ~/s38-api-step6/src: an unchanged file
# keeps its mtime (no rebuild), a changed one takes the copy's time (cargo sees it). Cargo
# runs under a user scope bounded to 16 GiB and 4 CPUs, with 4 jobs, its target directory
# outside the copy. `clean` removes all of it.
set -u
D=~/s38-api-step6
STAGE=${1:-tests}
if [ "$STAGE" = clean ]; then rm -rf "$D"; echo "removed $D"; exit 0; fi
mkdir -p "$D/src" "$D/logs"
rm -rf "$D/in" && mkdir -p "$D/in" && tar -x -C "$D/in" || { echo "unpack failed"; exit 2; }
rsync -r --checksum --delete "$D/in/" "$D/src/" || { echo "rsync failed"; exit 2; }
cd "$D/src" || exit 2
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$D/target" CARGO_TERM_COLOR=never
run() { # name, command...
  local name=$1; shift
  echo "== $name: $*"
  systemd-run --user --scope -q -p MemoryMax=16G -p CPUQuota=400% "$@" > "$D/logs/$name.log" 2>&1
  local rc=$?
  echo "== $name exit $rc"
  return $rc
}
summary() { grep -E '^test result|^test .* FAILED$|panicked at|^error(\[|:)|overflowed|^warning: unused' "$D/logs/$1.log" | tail -${2:-20}; }
uname -srm; rustc -V; date -u +%FT%TZ
case $STAGE in
  tests)
    run test-api cargo test -p shards-api -j4; summary test-api
    # The shards package has no library: its unit tests are its binary's. build::policy's
    # thread overflows its stack unoptimized (step 3's finding): test-release.
    run test-api-run cargo test -p shards --bin shards --profile test-release -j4 api_run; summary test-api-run;;
  clippy)
    run clippy cargo clippy -p shards-api -p shards --all-targets -j4 -- -D warnings
    grep -E '^(warning|error)' "$D/logs/clippy.log" | sort | uniq -c | head -30; tail -3 "$D/logs/clippy.log";;
  fmt)
    run fmt cargo fmt --all --check; tail -40 "$D/logs/fmt.log";;
  audit)
    python3 docs/research/measurements/s38-nested-grants/literal-audit.py > "$D/logs/audit.log" 2>&1; echo "audit exit $?"; tail -8 "$D/logs/audit.log";;
  mutants) # each line of step6/mutants.tsv: NAME, FILE, PERL-EXPR, CARGO-TEST-ARGS (tabs)
    while IFS=$'\t' read -r name file expr args <&3; do
      [ -z "$name" ] && continue
      case $name in \#*) continue;; esac
      cp "$file" "$D/orig"; perl -0pi -e "$expr" "$file"
      if cmp -s "$file" "$D/orig"; then echo "MUTANT $name NOT APPLIED"; continue; fi
      diff -u "$D/orig" "$file" > "$D/logs/mutant-$name.diff"
      touch "$file"
      run "mutant-$name" cargo test $args < /dev/null
      rc=$?
      echo "MUTANT $name: its test's exit $rc"; summary "mutant-$name" 6
      cp "$D/orig" "$file"; touch "$file"
    done 3< docs/research/measurements/s38-nested-grants/step6/mutants.tsv;;
  only)
    shift; run only "$@"; summary only 30;;
  logs) # prints a log, by name
    cat "$D/logs/$2.log";;
  *) echo "unknown stage $STAGE"; exit 2;;
esac
