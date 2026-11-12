#!/bin/bash
# round-kiln.sh REV STAGE...: as round.sh, through `kiln run --no-sync --stdin` to the kiln
# runner adacachy2 (Linux x86_64), which ssh does not reach. Used once adacachy's kiln
# headroom guard held this lane's builds to about 2 s of CPU a minute (2026-11-12, see
# logs/guard-adacachy.txt): the guard is its owner's protection, never bypassed. Each stage's
# output is kept in step6/logs/STAGE-REV.out.
set -u
W=/Users/adalundhe/Projects/shards/.claude/worktrees/s38-api
R=00aad8daff2fa3c9daa65d53c425d450
S=docs/research/measurements/s38-nested-grants/step6
KILN=/Users/adalundhe/.local/bin/kiln
REV=$1; shift
SHORT=$(git -C "$W" rev-parse --short=9 "$REV")
mkdir -p "$W/$S/logs"
"$KILN" run -q --on "$R" --no-sync --stdin -- 'cat > s38-api-remote.sh' < "$W/$S/s38-api-remote.sh" || exit 2
for stage in "$@"; do
  out="$W/$S/logs/$stage-$SHORT.out"
  {
    echo "#### stage $stage at $REV ($(date -u +%FT%TZ)), kiln runner $R"
    git -C "$W" archive "$REV" | "$KILN" run -q --on "$R" --no-sync --stdin -- "bash s38-api-remote.sh $stage" 2>&1 | grep -av '^skipping non-regular'
    echo "#### stage $stage exit ${PIPESTATUS[1]}"
  } | tee "$out"
done
