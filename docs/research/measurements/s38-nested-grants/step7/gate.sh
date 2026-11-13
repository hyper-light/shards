#!/bin/bash
# gate.sh REV RUNNER TARGET: nesting.rs's light gate (gate-remote.sh) for TARGET on the kiln
# runner RUNNER, over REV's files sent as a tar (`git archive`), so the runner builds exactly
# that revision. Its output is kept beside this script as gate-TARGET-REV.log.
set -u
W=/Users/adalundhe/Projects/shards/.claude/worktrees/s38-docs
S=$W/docs/research/measurements/s38-nested-grants/step7
KILN=/Users/adalundhe/.local/bin/kiln
rev=$1 runner=$2 target=$3
short=$(git -C "$W" rev-parse --short=9 "$rev") || exit 2
out=$S/gate-$target-$short.log
"$KILN" run -q --on "$runner" --no-sync --stdin -- 'cat > s38-docs-gate.sh' <"$S/gate-remote.sh" || exit 2
{
	echo "\$ git archive $(git -C "$W" rev-parse "$rev") | kiln run -q --on $runner --no-sync --stdin -- bash s38-docs-gate.sh $target"
	git -C "$W" archive "$rev" | "$KILN" run -q --on "$runner" --no-sync --stdin -- "bash s38-docs-gate.sh $target; e=\$?; rm -f s38-docs-gate.sh; exit \$e" 2>&1 |
		grep -av '^skipping non-regular'
	echo "kiln_exit=${PIPESTATUS[1]}"
} | tee "$out"
