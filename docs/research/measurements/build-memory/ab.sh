#!/usr/bin/env bash
# Interleaved before/after runs of the build run.sh makes (PM M80):
#   ab.sh BEFORE AFTER DIR [RUNS]
# BEFORE and AFTER are directories each holding a release `shards` and `shardsd`, and a
# `count/` directory with the same built with `--features alloc-count`. DIR holds
# `context/` (run.sh's) and `home0/`, a SHARDS_HOME with alpine:3.22 pulled, copied
# afresh (cloned where the file system clones) for every run. Each run's
# `/usr/bin/time -l`, `--progress=plain` step times and alloc-phase lines go to
# DIR/ab-{before,after}{,-count}.txt, which phases.py summarizes.
set -euo pipefail
before=$1 after=$2 dir=$3 runs=${4:-7}
one() { # BIN OUT ARGS...
  local bin=$1 out=$2; shift 2
  rm -rf "$dir/home"; cp -cR "$dir/home0" "$dir/home" 2>/dev/null || cp -R "$dir/home0" "$dir/home"
  SHARDS_HOME="$dir/home" /usr/bin/time -l "$bin/shards" build "$@" "$dir/context" >/dev/null 2>"$dir/last.err"
  { echo "=== $(date +%s) $bin"
    grep -E '#[0-9]+ DONE|alloc-phase|real|maximum resident|page faults|page reclaims|peak memory' "$dir/last.err"
  } >>"$out"
  rm -rf "$dir/home"
}
for _ in $(seq "$runs"); do
  for v in before after; do
    bin=${!v}
    one "$bin" "$dir/ab-$v.txt" --progress=plain
    one "$bin/count" "$dir/ab-$v-count.txt" -q
  done
done
