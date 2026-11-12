#!/usr/bin/env bash
# A real turn of one harness in a Shards microVM, signed in with the login this machine's
# user already has (managed-runtime plan G18: native sign-in delegation). Run it on the
# machine that holds the login, from this repository: the login never leaves the machine,
# and no value of it is printed, logged, committed or put in a synced folder.
#
#   signed-in.sh codex|claude BIN_ROOT WORK
#
# BIN_ROOT is fetch.sh's OUTDIR (harness-bringup-v1): its codex/ and claude/ hold the
# publishers' verified binaries. WORK, a new directory outside any synced repository,
# receives the test's records and output. The login file (Codex: ~/.codex/auth.json;
# Claude Code: ~/.claude/.credentials.json) is copied into a fresh 0700 directory on the
# user's tmpfs ($XDG_RUNTIME_DIR), the file 0600, and given to the harness alone
# (crates/shards/tests/harness_bringup.rs, `*_signed_in_*`). Whatever the test does, the
# exit trap then checks WORK for any secret of the login (in-process, found or not found),
# compares the copy with the original by hash and writes a refreshed copy back atomically
# where the original did not change meanwhile, and overwrites and removes the copy.
set -euo pipefail

harness=${1:?usage: signed-in.sh codex|claude BIN_ROOT WORK}
bins=${2:?}
work=${3:?}
here=$(cd "$(dirname "$0")" && pwd)
credentials() { python3 -I "$here/credentials.py" "$@"; }
case "$harness" in
codex)
    login=$HOME/.codex/auth.json share=codex-home file=auth.json
    variable=SHARDS_HARNESS_CODEX_HOME test=a_real_codex_signed_in_runs_a_tool_and_answers
    ;;
claude)
    login=$HOME/.claude/.credentials.json share=claude-home file=.credentials.json
    variable=SHARDS_HARNESS_CLAUDE_HOME test=a_real_claude_code_signed_in_answers
    ;;
*)
    echo "signed-in.sh: no harness $harness" >&2
    exit 2
    ;;
esac
test -s "$login" || { echo "signed-in.sh: $login is absent: this machine's user is not signed in" >&2; exit 2; }
mkdir "$work"
work=$(cd "$work" && pwd)
runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
private=$(mktemp -d -p "$runtime" shards-signed-in.XXXXXX)
chmod 0700 "$private"
echo "private copy on $(stat -f -c %T "$private") at $private"

settle() {
    local leak=0
    credentials leakcheck "$private" "$work" || leak=$?
    credentials settle "$private" || echo "signed-in.sh: settle failed: the original is untouched" >&2
    credentials scrub "$private" || echo "signed-in.sh: scrub failed: remove $private by hand" >&2
    if [ "$leak" -ne 0 ]; then
        echo "signed-in.sh: a secret is in $work: commit nothing of it" >&2
        exit 3
    fi
}
trap settle EXIT
# A stopped unit or a closed terminal sends one of these: exit normally, so that the copy
# is still settled and scrubbed.
trap 'exit 143' TERM INT HUP

credentials copy "$login" "$private" "$share" "$file"
status=0
start=$(date +%s)
env "SHARDS_HARNESS_${harness^^}=$bins/$harness" "$variable=$private/$share" \
    SHARDS_HARNESS_RECORDS="$work/records" \
    cargo test --profile test-release -p shards --test harness_bringup -- \
    --ignored --exact "$test" > "$work/test-output.log" 2>&1 || status=$?
echo "test $test: exit $status after $(($(date +%s) - start)) s"
# The summary and where a test failed, never a failure's message: it quotes the harness.
grep -E '^test result|panicked at [^ ]+:[0-9]+:[0-9]+:$' "$work/test-output.log" | cut -c1-200 || true
exit "$status"
