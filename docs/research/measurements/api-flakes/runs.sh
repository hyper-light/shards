#!/usr/bin/env bash
# Repeats shards-api's library tests on a runner (README.md).
#   runs.sh build SRC LABEL [TEST]
#                                builds SRC's (a checkout's) library test binary, or its
#                                integration test TEST's, once, at test-release, in a target
#                                directory of LABEL's own (one shared by two checkouts keeps
#                                the first's build: its fingerprints compare mtimes, not
#                                paths), and keeps a copy as ~/api-flakes/LABEL/api.exe
#   runs.sh loop LABEL N ARGS... runs that copy N times with libtest's ARGS, from SRC's
#                                crates/api as cargo runs it, keeping each failed run's log
#                                and writing a line a run to summary.txt: its number, its
#                                exit code, its seconds and the tests that failed. A run
#                                still going after RUN_LIMIT seconds (120: twice libtest's
#                                60 s warning, where whole runs take seconds) is ended, its
#                                tests then running named HUNG:<test>
# Bash 3.2 (macOS's /bin/bash) and Git Bash run it alike.
set -uo pipefail
out() { printf '%s/api-flakes/%s' "$HOME" "$1"; }
case ${1:-} in
build)
  src=$2
  dir=$(out "$3")
  mkdir -p "$dir"
  export CARGO_TARGET_DIR=$HOME/api-flakes-target/$3
  git -C "$src" rev-parse HEAD > "$dir/REVISION"
  git -C "$src" status --porcelain > "$dir/STATUS"
  printf '%s\n' "$src" > "$dir/SRC"
  target=--lib
  if [ -n "${4:-}" ]; then target="--test $4"; fi
  # shellcheck disable=SC2086 # the target is one or two words
  if ! (cd "$src" && cargo test -p shards-api $target --profile test-release --no-run \
    --message-format=json) > "$dir/build.json" 2> "$dir/build.log"; then
    tail -40 "$dir/build.log"
    exit 1
  fi
  # The one executable the build made, JSON's escaped backslashes undone.
  exe=$(grep -o '"executable":"[^"]*"' "$dir/build.json" | tail -1 |
    sed -e 's/^"executable":"//' -e 's/"$//' -e 's/\\\\/\\/g')
  if command -v cygpath > /dev/null; then exe=$(cygpath -u "$exe"); fi
  cp "$exe" "$dir/api.exe.tmp" && mv "$dir/api.exe.tmp" "$dir/api.exe"
  (cd "$dir" && { shasum -a 256 api.exe 2> /dev/null || sha256sum api.exe; } > api.exe.sha256)
  printf 'built %s at %s: %s\n' "$3" "$(cat "$dir/REVISION")" "$(cat "$dir/api.exe.sha256")"
  ;;
loop)
  label=$2
  dir=$(out "$label")
  n=$3
  shift 3
  src=$(cat "$dir/SRC")
  : > "$dir/summary.txt"
  printf '%s\n' "$*" > "$dir/ARGS"
  i=1
  while [ "$i" -le "$n" ]; do
    began=$(date +%s)
    if command -v timeout > /dev/null; then
      (cd "$src/crates/api" && timeout "${RUN_LIMIT:-120}" "$dir/api.exe" "$@") > "$dir/run-$i.log" 2>&1
    else
      # macOS has no timeout(1): an alarm outlives exec, and its signal ends the run.
      (cd "$src/crates/api" && perl -e 'alarm shift; exec @ARGV or die "exec: $!"' \
        "${RUN_LIMIT:-120}" "$dir/api.exe" "$@") > "$dir/run-$i.log" 2>&1
    fi
    code=$?
    took=$(($(date +%s) - began))
    failed=$(sed -n -e 's/^test \(.*\) \.\.\. FAILED$/\1/p' \
      -e 's/^test \(.*\) has been running for over 60 seconds$/HUNG:\1/p' "$dir/run-$i.log" | tr '\n' ' ')
    printf '%s %s %ss %s\n' "$i" "$code" "$took" "$failed" >> "$dir/summary.txt"
    if [ "$code" = 0 ]; then rm -f "$dir/run-$i.log"; fi
    i=$((i + 1))
  done
  runs=$(wc -l < "$dir/summary.txt" | tr -d ' ')
  bad=$(awk '$2 != 0' "$dir/summary.txt" | wc -l | tr -d ' ')
  printf '%s: %s of %s runs failed\n' "$label" "$bad" "$runs"
  awk '{for (i = 4; i <= NF; i++) print $i}' "$dir/summary.txt" | sort | uniq -c | sort -rn
  ;;
*)
  sed -n '2,15p' "$0"
  exit 2
  ;;
esac
