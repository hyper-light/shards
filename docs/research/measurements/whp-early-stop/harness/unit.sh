#!/bin/bash
# The fix's unit tests, green, then each guard removed (red), on this host: usage
#   bash unit.sh WORK COMMIT [full]
# WORK: a directory of its own (a partial clone of shards, its target, its logs); COMMIT the
# fix's, fetched from origin's p/whp-early-stop-v1. With `full`, the shards binary's whole
# unit suite runs too, --no-fail-fast. Each step's output is kept in WORK/logs.
set -u
work=${1:?work}; want=${2:?commit}; full=${3:-}
mkdir -p "$work/logs"
src=$work/src
logs=$work/logs
export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=gc.auto GIT_CONFIG_VALUE_0=0 GIT_CONFIG_KEY_1=maintenance.auto GIT_CONFIG_VALUE_1=false
if [ ! -d "$src/.git" ]; then
  for i in 1 2 3; do
    rm -rf "$src"
    git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/shards.git "$src" && break
    sleep 20
  done
fi
for i in 1 2 3; do git -C "$src" fetch -q origin "+refs/heads/p/whp-early-stop-v1:refs/remotes/origin/p/whp-early-stop-v1" && break; sleep 20; done
git -C "$src" checkout -q --detach "$want" || exit 1
git -C "$src" log -1 --format='HEAD %H %s' | cut -c1-110
test -z "$(git -C "$src" status --porcelain)" || { echo "dirty tree"; exit 1; }
export CARGO_TARGET_DIR=$work/target SHARDS_HELPERS=skip
: > "$work/empty"
empty=$work/empty
case "$(uname -s)" in MINGW* | MSYS*) empty=$(cygpath -w "$empty") ;; esac
export SHARDS_INIT_BINARY=$empty SHARDS_SERVER_BINARY=$empty
cd "$src" || exit 1
echo "host: $(uname -s) $(uname -m) $(uname -r | cut -c1-40); $(rustc --version)"
tests="daemon::tests::a_side_that_ends_without_its_last_word_says_so daemon::tests::a_client_hears_its_session_end_though_it_says_nothing daemon::tests::a_client_whose_request_never_went_hears_its_restart cli::client::tests::a_runs_client_tells_its_daemons_end_from_its_microvms"
step() {
  local name=$1; shift
  local t0=$(date +%s)
  "$@" > "$logs/$name.log" 2>&1
  local rc=$?
  echo "$name: exit $rc in $(( $(date +%s) - t0 )) s: $(grep -a -E '^test result' "$logs/$name.log" | tail -1)"
  grep -a -E '^test .* \.\.\. FAILED|panicked at|left:|right:|assertion' "$logs/$name.log" | head -8
  return $rc
}
step ipc-kinds cargo test --locked -p shards-ipc --lib -- every_message_kind_has_its_own_number
step green cargo test --locked -p shards --bin shards -- $tests
# Guard A: the relay says no UNANSWERED (main's relay): the daemon's test fails.
slots=crates/shards/src/daemon/slots.rs
cp "$slots" "$work/slots.rs.orig"
sed -i.bak 's/let _ = living.answer(&answer, kind::UNANSWERED, &\[\]);/let _ = (\&answer, kind::UNANSWERED);/' "$slots"
grep -c 'let _ = (&answer, kind::UNANSWERED);' "$slots" | sed 's/^/mutant A lines: /'
step red-relay cargo test --locked -p shards --bin shards -- daemon::tests::a_side_that_ends_without_its_last_word_says_so
cp "$work/slots.rs.orig" "$slots"; rm -f "$slots.bak"
# Guard B: the client takes a wordless end for its microVM's (main's words): the client's test fails.
client=crates/shards/src/cli/client.rs
cp "$client" "$work/client.rs.orig"
# The product's line alone (the test names the same words beside its own arguments).
sed -i.bak 's/^\( *\)"the daemon ended before the command did; see {}",$/\1"the command'"'"'s microVM stopped before the command ended; see {}",/' "$client"
git diff --stat -- "$client" | sed 's/^/mutant B: /'
step red-client cargo test --locked -p shards --bin shards -- cli::client::tests::a_runs_client_tells_its_daemons_end_from_its_microvms
cp "$work/client.rs.orig" "$client"; rm -f "$client.bak"
touch "$slots" "$client"
test -z "$(git status --porcelain)" && echo "tree restored clean" || git status --porcelain
if [ "$full" = full ]; then
  # The test-release profile, as CI's unit tests run: on adacachy the dev build's run ended
  # at a stack overflow of a test's 'policy' thread (Rego), code this change leaves alone.
  step full cargo test --locked --profile test-release -p shards --bin shards --no-fail-fast
fi
echo "done"
