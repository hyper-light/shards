#!/bin/sh
# Every open run of this repository's workflows, and whether a newer push made it moot (PM M539).
# Run from the checkout, after `git fetch origin --prune`; needs gh and git.
#
#   open-runs.sh > open.tsv
#
# Columns: run, status, workflow, branch, commit, created, and why it is moot: `newer` (a
# newer run of its workflow and branch is open), `on-main` (its commit is on main, and its
# branch is not main), `branch-gone` (its branch was deleted), or `-`.
set -eu
gh run list --limit 1000 \
	--json databaseId,status,workflowName,headBranch,headSha,createdAt \
	--jq '.[] | select(.status != "completed")
		| [.databaseId, .status, .workflowName, .headBranch, .headSha, .createdAt] | @tsv' |
	sort -t "$(printf '\t')" -k6,6r |
	while IFS="$(printf '\t')" read -r id status workflow branch sha created; do
		key="$workflow/$branch"
		why=-
		if printf '%s\n' "${seen:-}" | grep -qxF "$key"; then
			why=newer
		elif ! git rev-parse -q --verify "origin/$branch" >/dev/null; then
			why=branch-gone
		elif [ "$branch" != main ] && git merge-base --is-ancestor "$sha" origin/main 2>/dev/null; then
			why=on-main
		fi
		seen="${seen:-}
$key"
		printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "$status" "$workflow" "$branch" "$sha" "$created" "$why"
	done
