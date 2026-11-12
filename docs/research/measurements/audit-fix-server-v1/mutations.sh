#!/usr/bin/env bash
# Runs guard-removal mutations of audit-fix-server-v1 (mutate.py), each against the tests that
# must fail without its guard, restoring each mutated file (and its mtime, newer than any
# build of the mutant, so that cargo rebuilds it) before the next. From the repository root:
#   docs/research/measurements/audit-fix-server-v1/mutations.sh M1a M1b M2 ...
# Each line of output is the run's raw output; a mutation's tests passing is a surviving mutant.
set -u
here=docs/research/measurements/audit-fix-server-v1
declare -A tests=(
  [M1a]="a_program_answers_a_request_that_closes_its_connection"
  [M1b]="a_program_answers_a_request_that_closes_its_connection"
  [M2]="a_stop_the_helper_cannot_yet_take_reaches_it_once_it_can a_gone_clients_program_is_stopped_though_the_helper_was_full"
  [M3]="a_refusal_without_storage_says_it_was_refused"
  [M4]="a_small_hand_reserves_what_its_file_holds"
  [M4b]="a_file_longer_than_its_size_says_is_handed_as_before"
  [M5a]="a_programs_broker_results_keep_their_receipt"
  [M5b]="a_programs_broker_results_keep_their_receipt"
  [M6]="a_worker_not_reaped_ends_no_other"
)
backup=$(mktemp)
for m in "$@"; do
  file=$(python3 "$here/mutate.py" --file "$m") || exit 2
  cp "$file" "$backup"
  echo "=== $m: $file"
  python3 "$here/mutate.py" "$m" >/dev/null || exit 2
  touch "$file"
  diff -u "$backup" "$file" | tail -n +3
  # shellcheck disable=SC2086
  cargo test -p shards-server --target x86_64-unknown-linux-gnu -j2 -- --nocapture ${tests[$m]} 2>&1 \
    | grep -v '^warning: aws-lc-sys'
  echo "=== $m: cargo test exit ${PIPESTATUS[0]} (non-zero: the mutant is killed)"
  cp "$backup" "$file"
  touch "$file"
  cmp -s "$backup" "$file" && echo "=== $m: $file restored"
done
rm -f "$backup"
echo "=== source after: $(git diff --stat -- crates/server | tail -1)"
