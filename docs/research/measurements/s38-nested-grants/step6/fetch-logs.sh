#!/bin/bash
# fetch-logs.sh REV: copies the runner's cargo logs of REV's round (~/s38-api-step6/logs on
# adacachy2) into step6/logs/remote-REV/, as base64 of a tar through kiln run.
set -u
W=/Users/adalundhe/Projects/shards/.claude/worktrees/s38-api
R=00aad8daff2fa3c9daa65d53c425d450
S=docs/research/measurements/s38-nested-grants/step6
KILN=/Users/adalundhe/.local/bin/kiln
SHORT=$(git -C "$W" rev-parse --short=9 "$1")
dest="$W/$S/logs/remote-$SHORT"
mkdir -p "$dest"
"$KILN" run -q --on "$R" --no-sync -- 'tar -C ~/s38-api-step6/logs -cz . | base64 -w0' < /dev/null |
  base64 -d | tar -xz -C "$dest" && ls -la "$dest"
