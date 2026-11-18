#!/usr/bin/env bash
# Red and green for durable-v2's time each write took and the judge's hold its subject measured
# (#36, own-35i's commit 2; D286 amended): the light tests that hold them, at the checkout
# (green) and with each red patch beside this script applied alone (red), in this checkout,
# which it leaves as it found it. Run from the repository's root:
#   docs/research/measurements/durable-spent-v1/run.sh
# Output goes to green.log and red-NAME.log beside this script. Expected reds:
#   red-spent     shards-durable a_slot_counts_the_time_its_writes_took (its spent stays 0)
#   red-idle      shards-durable an_ended_threads_slot_is_its_processs_next_threads (freed)
#   red-hosted    shards-judge its_hosts_hold_its_subject_measured_is_out_of_its_stillness_and_its_rate
#   red-withheld  shards-api a_windows_withholding_is_the_most_any_one_thread_waited (a sum)
#   red-counted   shards-api a_look_counts_the_most_any_slot_of_its_party_held_it (a sum)
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
run() {
  echo "HEAD=$(git rev-parse HEAD) host=$(hostname) $(uname -srm) $(rustc --version)"
  git status --short
  cargo test --profile test-release -p shards-durable -p shards-judge 2>&1
  cargo test --profile test-release -p shards-api --lib -- \
    a_look_counts_the_most_any_slot_of_its_party_held_it \
    a_windows_withholding_is_the_most_any_one_thread_waited 2>&1
}
run > "$here/green.log"
green=$?
echo "green: exit $green (0 expected)"
status=$((green != 0))
for red in spent idle hosted withheld counted; do
  if ! git apply "$here/red-$red.patch"; then
    echo "red-$red: does not apply"
    status=1
    continue
  fi
  run > "$here/red-$red.log"
  code=$?
  git apply -R "$here/red-$red.patch"
  echo "red-$red: exit $code (non-zero expected; its log says which assertion)"
  [ "$code" -ne 0 ] || status=1
done
exit "$status"
