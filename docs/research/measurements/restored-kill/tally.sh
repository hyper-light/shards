#!/usr/bin/env bash
# The checkpoint test's result in each completed x86_64 Windows job of CI on main
# (PM M538): tally.sh [PAGES], from a clone of shards with origin/main fetched.
#
# Scans main's last PAGES x 100 commits (2 if none given) through GitHub's GraphQL API,
# fetches each completed job's log once (REST, one call a job) into $LOGS
# ($TMPDIR/restored-kill-logs if unset), and prints, tab-separated: the commit, when it
# was committed, the job, its runner image, `the_docker_cli_checkpoints_and_restores_microvms`'s
# result (none where the job never ran engine.rs), how many of its `rm -f` timed out, and
# whether the signal link's fix (3607aad11, on main since 1c349a632) is in the commit.
# Then the counts of each.
set -u
pages=${1:-2}
fix=3607aad11
logs=${LOGS:-${TMPDIR:-/tmp}/restored-kill-logs}
mkdir -p "$logs"
jobs=$(mktemp)
after=""
for _ in $(seq "$pages"); do
  cursor=""
  [ -n "$after" ] && cursor=", after: \"$after\""
  page=$(gh api graphql -f query="query { repository(owner: \"hyper-light\", name: \"shards\") {
    ref(qualifiedName: \"refs/heads/main\") { target { ... on Commit {
      history(first: 100$cursor) { pageInfo { endCursor hasNextPage } nodes { oid committedDate
        checkSuites(first: 15) { nodes { workflowRun { workflow { name } }
          checkRuns(first: 30) { nodes { name status databaseId } } } } } } } } } } }") || exit 1
  printf '%s' "$page" | jq -r '.data.repository.ref.target.history.nodes[] | .oid as $o | .committedDate as $d
    | .checkSuites.nodes[] | select(.workflowRun.workflow.name == "CI") | .checkRuns.nodes[]
    | select((.name | startswith("x86_64-pc-windows-msvc")) and .status == "COMPLETED")
    | [$o[0:9], $d, (.databaseId | tostring)] | @tsv' >> "$jobs"
  [ "$(printf '%s' "$page" | jq -r '.data.repository.ref.target.history.pageInfo.hasNextPage')" = true ] || break
  after=$(printf '%s' "$page" | jq -r '.data.repository.ref.target.history.pageInfo.endCursor')
done
rows=$(mktemp)
sort -u -t$'\t' -k3,3 "$jobs" | while IFS=$'\t' read -r sha date id; do
  log="$logs/$id.txt"
  if [ ! -s "$log" ]; then
    gh api "repos/hyper-light/shards/actions/jobs/$id/logs" 2>/dev/null |
      sed -E 's/\x1b\[[0-9;]*[mK]//g' > "$log"
  fi
  result=$(grep -a -E 'test the_docker_cli_checkpoints_and_restores_microvms \.\.\. ' "$log" |
    sed -E 's/.*\.\.\. //' | tr -d '\r' | tr '\n' ' ' | sed 's/ $//')
  timeouts=$(grep -a -c '"rm" "-f" "cp" did not exit' "$log")
  image=$(grep -a -m1 -E ' Image: ' "$log" | sed -E 's/.*Image: //' | tr -d '\r')
  if git merge-base --is-ancestor "$fix" "$sha" 2>/dev/null; then fixed=yes; else fixed=no; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$sha" "$date" "$id" "${image:-?}" "${result:-none}" "$timeouts" "$fixed"
done | sort -t$'\t' -k2,2 > "$rows"
printf 'commit\tcommitted\tjob\timage\tcheckpoint_test\trm_f_timeouts\tsignal_link_fix_in\n'
cat "$rows"
printf '\ncount\tfix in\timage\tresult\n'
awk -F'\t' '{print $7 "\t" $4 "\t" $5}' "$rows" | sort | uniq -c | sed -E 's/^ +//; s/ /\t/'
rm -f "$jobs" "$rows"
