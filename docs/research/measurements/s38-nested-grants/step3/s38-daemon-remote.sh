#!/bin/bash
# s38-daemon-remote.sh STAGE [ARGS]: D234 step 3's light checks (lane p/s38-daemon-v1).
# stdin: a `git archive` of the revision to check. It is unpacked into ~/s38-daemon-test/in,
# then copied by content over ~/s38-daemon-test/src: an unchanged file keeps its mtime (no
# rebuild), a changed one takes the copy's time (cargo sees it). Cargo runs under a user
# scope bounded to 16 GiB and 4 CPUs, with 4 jobs.
set -u
D=~/s38-daemon-test
STAGE=${1:-tests}
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
summary() { grep -E '^test result|^test .* FAILED$|panicked at|^error(\[|:)|overflowed' "$D/logs/$1.log" | tail -${2:-20}; }
uname -srm; rustc -V
case $STAGE in
  tests)
    run test-ipc cargo test -p shards-ipc -j4; summary test-ipc
    # build::policy's `policy` thread overflows its stack in the unoptimized profile, as at
    # the base: its tests run in `release-tests`.
    run test-shards cargo test -p shards --bin shards -j4 -- --test-threads 8 --skip build::policy; summary test-shards
    run test-vm cargo test -p shards-vm-process --bin shards-vm -j4; summary test-vm;;
  release-tests)
    run rtest-ipc cargo test -p shards-ipc --profile test-release -j4; summary rtest-ipc
    run rtest-shards cargo test -p shards --bin shards --profile test-release -j4 -- --test-threads 8; summary rtest-shards 30
    run rtest-vm cargo test -p shards-vm-process --bin shards-vm --profile test-release -j4; summary rtest-vm;;
  clippy)
    run clippy cargo clippy -p shards -p shards-ipc -p shards-vm-process --all-targets -j4 -- -D warnings
    grep -E '^(warning|error)' "$D/logs/clippy.log" | sort | uniq -c | head -30; tail -3 "$D/logs/clippy.log";;
  fmt)
    run fmt cargo fmt --all -- --check; tail -20 "$D/logs/fmt.log";;
  audit)
    python3 docs/research/measurements/s38-nested-grants/literal-audit.py > "$D/logs/audit.log" 2>&1; echo "audit exit $?"; tail -5 "$D/logs/audit.log";;
  mutants) # each line of ~/s38-mutants.tsv: NAME, FILE, PERL-EXPR, CARGO-TEST-ARGS (tabs)
    while IFS=$'\t' read -r name file expr args <&3; do
      [ -z "$name" ] && continue
      cp "$file" "$D/orig"; perl -0pi -e "$expr" "$file"
      if cmp -s "$file" "$D/orig"; then echo "MUTANT $name NOT APPLIED"; continue; fi
      diff -u "$D/orig" "$file" > "$D/logs/mutant-$name.diff"
      touch "$file"
      run "mutant-$name" cargo test $args < /dev/null
      rc=$?
      echo "MUTANT $name: its test's exit $rc"; summary "mutant-$name" 4
      cp "$D/orig" "$file"; touch "$file"
    done 3< ~/s38-mutants.tsv;;
  mutant) # NAME FILE PERL-EXPR CARGO-TEST-ARGS...
    name=$2; file=$3; expr=$4; shift 4
    cp "$file" "$D/orig"; perl -0pi -e "$expr" "$file"
    if cmp -s "$file" "$D/orig"; then echo "MUTANT $name NOT APPLIED"; cp "$D/orig" "$file"; touch "$file"; exit 3; fi
    diff -u "$D/orig" "$file" | head -20
    touch "$file"
    run "mutant-$name" cargo test "$@"
    rc=$?
    summary "mutant-$name" 12
    cp "$D/orig" "$file"; touch "$file"
    echo "MUTANT $name test exit $rc (restored)";;
  only)
    shift; run only "$@"; tail -40 "$D/logs/only.log";;
  *) echo "unknown stage $STAGE"; exit 2;;
esac
