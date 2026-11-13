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
# receives the test's records and output. The login file is where each harness keeps it on
# every system it runs on: Codex's auth.json in CODEX_HOME (~/.codex; its own default store
# is that file, codex-rs/config/defaults.toml `cli_auth_credentials_store = "file"` at
# rust-v0.167.2), Claude Code's .credentials.json in CLAUDE_CONFIG_DIR (~/.claude). It is
# copied into a fresh 0700 directory of the user's own, the file 0600, and given to the
# harness alone (crates/shards/tests/harness_bringup.rs, `*_signed_in_*`). Whatever the test
# does, the exit trap then checks WORK for any secret of the login (in-process, found or not
# found), compares the copy with the original by hash and writes a refreshed copy back in one
# step where the original did not change meanwhile, and overwrites and removes the copy.
#
# Where the copy goes is each system's own: Linux's per-user tmpfs ($XDG_RUNTIME_DIR, memory
# alone); macOS's per-user temporary directory ($TMPDIR, else confstr's DARWIN_USER_TEMP_DIR,
# 0700 to its user), which is on disk,
# as macOS gives a user no memory-backed file system (a RAM disk, hdiutil's ram://, takes a
# size fixed before the harness writes its state, hundreds of files: v2's S3). Claude Code
# keeps a macOS sign-in in the Keychain and writes this file there only when the Keychain
# refuses it (code.claude.com/docs/en/iam, "Credential management"); this script reads no
# Keychain, so on macOS it takes that file or nothing. Windows is refused: credentials.py
# guards the copy by its mode, and Python's os.chmod sets only Windows' read-only flag
# (docs.python.org, os.chmod), so it could neither make nor check the copy's protection.
set -euo pipefail

harness=${1:?usage: signed-in.sh codex|claude BIN_ROOT WORK}
bins=${2:?}
work=${3:?}
here=$(cd "$(dirname "$0")" && pwd)
credentials() { python3 -I "$here/credentials.py" "$@"; }
case "$harness" in
codex)
    login=${CODEX_HOME:-$HOME/.codex}/auth.json share=codex-home file=auth.json
    variable=SHARDS_HARNESS_CODEX_HOME test=a_real_codex_signed_in_runs_a_tool_and_answers
    ;;
claude)
    login=${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json share=claude-home file=.credentials.json
    variable=SHARDS_HARNESS_CLAUDE_HOME test=a_real_claude_code_signed_in_answers
    ;;
*)
    echo "signed-in.sh: no harness $harness" >&2
    exit 2
    ;;
esac
system=$(uname -s)
case $system in
Linux) runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} ;;
Darwin) runtime=${TMPDIR:-$(getconf DARWIN_USER_TEMP_DIR)} ;;
*)
    echo "signed-in.sh: $system: credentials.py guards the copy by its mode, which Python cannot set or check here (os.chmod sets Windows' read-only flag alone)" >&2
    exit 2
    ;;
esac
if ! [ -s "$login" ]; then
    if [ "$system" = Darwin ] && [ "$harness" = claude ]; then
        echo "signed-in.sh: $login is absent: on macOS Claude Code keeps its sign-in in the Keychain, which this script does not read" >&2
    else
        echo "signed-in.sh: $login is absent: this machine's user is not signed in" >&2
    fi
    exit 2
fi
mkdir "$work"
work=$(cd "$work" && pwd)
private=$(mktemp -d "${runtime%/}/shards-signed-in.XXXXXX")
chmod 0700 "$private"
echo "private copy on $(df -P "$private" | awk 'NR == 2 {print $1}') at $private"

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
upper=$(printf '%s' "$harness" | tr '[:lower:]' '[:upper:]')
env "SHARDS_HARNESS_$upper=$bins/$harness" "$variable=$private/$share" \
    SHARDS_HARNESS_RECORDS="$work/records" \
    cargo test --profile test-release -p shards --test harness_bringup -- \
    --ignored --exact "$test" > "$work/test-output.log" 2>&1 || status=$?
echo "test $test: exit $status after $(($(date +%s) - start)) s"
# The summary and where a test failed, never a failure's message: it quotes the harness.
grep -E '^test result|panicked at [^ ]+:[0-9]+:[0-9]+:$' "$work/test-output.log" | cut -c1-200 || true
exit "$status"
