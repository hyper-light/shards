#!/usr/bin/env bash
# Red and green for the harness's party members (#36, own-35i; D215; CI 44347282760's
# windows-2025 refusals at context.rs:477, a join's daemon not followed, and plug.rs:305, an
# exec's home's daemon not followed): the harness tests that hold them, at the checkout
# (green) and with each red patch beside this script applied alone (red), in this checkout,
# which it leaves as it found it. Run from the repository's root, on any host:
#   docs/research/measurements/harness-party-members-v1/run.sh
# Each run's output goes to green.log and red-NAME.log beside this script. Expected:
#   green          every test passes
#   red-invite     a_join_is_served_by_the_home_its_invitation_names fails at "create context"
#   red-kept-here  the same test fails at ["use", "context", "gpu"] "runs here"
#   red-vm-home    a_guests_wait_through_a_homes_client_follows_its_daemon fails, its stand-in
#                  saying "running: ..." with no "no daemon yet in" its home
#   red-vm-declares  the same test fails, its stand-in's account of the command that names no
#                  home reading its declarations in this process's home, not its command's
#                  (own-35i's commit 1b, D286 amended)
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tests=(
  a_join_is_served_by_the_home_its_invitation_names
  a_guests_wait_through_a_homes_client_follows_its_daemon
  a_contexts_command_is_served_by_the_home_that_listens_on_its_address
)
run() {
  echo "HEAD=$(git rev-parse HEAD) host=$(hostname) $(uname -srm) $(rustc --version)"
  git status --short
  cargo test -p shards --profile test-release --test harness -- --exact "${tests[@]}" 2>&1
}
run > "$here/green.log"
green=$?
echo "green: exit $green (0 expected)"
status=$((green != 0))
for red in invite kept-here vm-home vm-declares; do
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
