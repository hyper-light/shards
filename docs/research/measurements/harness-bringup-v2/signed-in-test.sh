#!/usr/bin/env bash
# signed-in.sh held to a stand-in turn, with no VM, binary, network or credential of anyone:
#
#   signed-in-test.sh [SCRIPT]    SCRIPT: the signed-in.sh to hold, by default this one's
#
# A fake Codex login (digests of labels, no provider's) sits in a home of the test's own. A
# stand-in `cargo` plays the turn: it notes the copy it was given, its mode and its
# directory's, and refreshes its access token there, as a harness whose token neared its
# expiry would. signed-in.sh passes where it gives the harness a 0600 copy in a 0700 directory
# of this system's own store, never the original; ends with the stand-in's exit; finds none of
# the login in WORK; writes the refresh back over the fake login in one step, its mode kept;
# and scrubs the copy. On macOS, a Claude Code login it can't take (one in the Keychain) is
# refused, saying so.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
script=${1:-$here/signed-in.sh}
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
unset CODEX_HOME CLAUDE_CONFIG_DIR
fake() {
    if command -v sha256sum > /dev/null; then
        printf '%s' "$1" | sha256sum
    else
        printf '%s' "$1" | shasum -a 256
    fi | cut -d' ' -f1
}
mkdir -p "$t/home/.codex" "$t/bin" "$t/bins/codex"
chmod 0700 "$t/home/.codex"
printf '{"auth_mode": "chatgpt", "tokens": {"access_token": "%s", "refresh_token": "%s", "account_id": "%s"}, "last_refresh": "2026-11-01T00:00:00Z"}\n' \
    "$(fake access)" "$(fake refresh)" "$(fake account)" > "$t/home/.codex/auth.json"
chmod 0600 "$t/home/.codex/auth.json"
cat > "$t/bin/cargo" <<'EOF'
#!/usr/bin/env bash
# The stand-in turn: the copy given the harness noted, then its access token refreshed.
python3 -I - "${SHARDS_HARNESS_CODEX_HOME:?}/auth.json" "$STANDIN/given" <<'PY'
import json, os, stat, sys
path, out = sys.argv[1], sys.argv[2]
modes = (stat.S_IMODE(os.stat(path).st_mode), stat.S_IMODE(os.stat(os.path.dirname(path)).st_mode))
with open(path) as f:
    doc = json.load(f)
doc["tokens"]["access_token"] = "refreshed-" + doc["tokens"]["access_token"][::-1]
with open(path, "w") as f:
    json.dump(doc, f)
with open(out, "w") as f:
    f.write("%s %o %o\n" % ((os.path.dirname(path),) + modes))
PY
mkdir -p "$SHARDS_HARNESS_RECORDS"
echo "a record of the turn, holding nothing of the login" > "$SHARDS_HARNESS_RECORDS/stdout.bin"
echo "test result: ok. 1 passed"
EOF
chmod +x "$t/bin/cargo"
failed=0
fail() {
    echo "FAIL ($1): $2"
    sed 's/^/  | /' "$t/said-$1" 2> /dev/null | head -12
    failed=1
}

status=0
HOME=$t/home STANDIN=$t PATH="$t/bin:$PATH" bash "$script" codex "$t/bins" "$t/work-codex" \
    > "$t/said-codex" 2>&1 || status=$?
case $(uname -s) in
Linux) store=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} ;;
*) store=${TMPDIR:-$(getconf DARWIN_USER_TEMP_DIR)} ;;
esac
read -r given mode dirmode 2> /dev/null < "$t/given" || given=
if [ "$status" -ne 0 ]; then
    fail codex "signed-in.sh exit $status"
elif [[ $given != "${store%/}"/shards-signed-in.* ]]; then
    fail codex "the harness was given $given, not a copy in ${store%/}"
elif [ "$mode $dirmode" != "600 700" ]; then
    fail codex "the copy was $mode in a $dirmode directory"
elif [ -e "$given" ] || [ -e "$(dirname "$given")" ]; then
    fail codex "the copy outlived the run: $(dirname "$given")"
elif ! grep -q "leak check: .* not found" "$t/said-codex"; then
    fail codex "the leak check did not say not found"
elif ! grep -q "written back over the original in one step" "$t/said-codex" ||
    ! grep -q '"access_token": "refreshed-' "$t/home/.codex/auth.json"; then
    fail codex "the refresh was not written back"
elif [ "$(stat -f %Lp "$t/home/.codex/auth.json" 2> /dev/null || stat -c %a "$t/home/.codex/auth.json")" != 600 ]; then
    fail codex "the login's mode changed"
elif ls -A "$t/home/.codex" | grep -q '^\.shards-writeback-'; then
    fail codex "a write-back's temporary file was left"
else
    echo "PASS (codex): a 0600 copy in a 0700 directory of ${store%/}, the refresh written back in one step, the copy scrubbed"
fi

if [ "$(uname -s)" = Darwin ]; then
    status=0
    HOME=$t/home STANDIN=$t PATH="$t/bin:$PATH" bash "$script" claude "$t/bins" "$t/work-claude" \
        > "$t/said-claude" 2>&1 || status=$?
    if [ "$status" -eq 2 ] && grep -q "keeps its sign-in in the Keychain" "$t/said-claude"; then
        echo "PASS (claude): a macOS sign-in in the Keychain refused, saying so"
    else
        fail claude "exit $status"
    fi
fi
exit "$failed"
