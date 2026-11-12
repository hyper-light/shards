#!/bin/bash
# round.sh REV STAGE...: runs s38-api-remote.sh's stages on adacachy (Linux x86_64), each on
# `git archive REV` of this worktree, its output kept in step6/logs/STAGE-REV.out, then the
# remote log of each cargo run copied beside it. REV is a commit, or a tree from
# `git write-tree` for a check before one.
set -u
W=/Users/adalundhe/Projects/shards/.claude/worktrees/s38-api
H=adalundhe@192.168.1.77
S=docs/research/measurements/s38-nested-grants/step6
REV=$1; shift
SHORT=$(git -C "$W" rev-parse --short=9 "$REV")
mkdir -p "$W/$S/logs"
ssh -o BatchMode=yes "$H" "cat > s38-api-remote.sh" < "$W/$S/s38-api-remote.sh" || exit 2
for stage in "$@"; do
  out="$W/$S/logs/$stage-$SHORT.out"
  {
    echo "#### stage $stage at $REV ($(date -u +%FT%TZ)), host $H"
    git -C "$W" archive "$REV" | ssh -o BatchMode=yes -o ServerAliveInterval=60 "$H" "bash s38-api-remote.sh $stage" 2>&1 | grep -av '^skipping non-regular'
    echo "#### stage $stage exit ${PIPESTATUS[1]}"
  } | tee "$out"
done
