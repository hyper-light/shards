#!/bin/bash
# Plan step 33, step 8's evidence: what Claude Code 2.1.311's pinned linux/amd64 bundle and Codex
# rust-v0.167.2's login sources say of their sign-in, refresh, revoke, ID token, at_hash and DPoP.
# Each is held to its pin before it is read; neither is ever run. Writes each section to
# $T/out/<name>.txt and its whole output to $T/evidence.log, then prints the sections.
#   bash evidence.sh
set -u
export LC_ALL=C
T=$HOME/g121-tmp/s33-signin/evidence; O=$T/out
mkdir -p "$T" "$O"
log=$T/evidence.log
{
echo "HOST $(hostname) $(uname -sm) $(date -u +%FT%TZ)"
cd "$T" || exit 2

# --- Claude Code 2.1.311, linux-x64 (gnu): its pin (kiln .kiln/images/harness-claude/pin.json).
B=$T/claude
want=86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada
if [ ! -f "$B" ] || [ "$(sha256sum "$B" | cut -d' ' -f1)" != "$want" ]; then
  curl -sSfL -A curl/8.23.0 -o "$B" https://downloads.claude.ai/claude-code-releases/2.1.311/linux-x64/claude || { echo "CLAUDE FETCH FAILED"; exit 2; }
fi
got=$(sha256sum "$B" | cut -d' ' -f1)
echo "CLAUDE sha256 $got (pin $want) size $(stat -c %s "$B")"
[ "$got" = "$want" ] || { echo "CLAUDE SHA MISMATCH"; exit 2; }

ctx() { # ctx NAME BEFORE AFTER REGEX [MAX]: each match with its context, offsets kept
  local name=$1 before=$2 after=$3 re=$4 max=${5:-40}
  { echo "## command: grep -a -o -b -E '.{0,$before}$re.{0,$after}' claude | head -$max"
    grep -a -o -b -E ".{0,$before}$re.{0,$after}" "$B" | tr -c '[:print:]\n' '.' | head -"$max"
    echo "## end"; } >> "$O/claude-$name.txt"
}
cnt() { # cnt NAME REGEX: the count of matches, at and past the app payload's start
  local name=$1 re=$2
  { echo "## command: grep -a -o -b -E '$re' claude | count all / at or past 115363840"
    echo "all $(grep -a -o -b -E "$re" "$B" | wc -l) payload $(grep -a -o -b -E "$re" "$B" | awk -F: '$1 >= 115363840' | wc -l)"
    echo "## end"; } >> "$O/claude-$name.txt"
}
rm -f "$O"/claude-*.txt
cnt at_hash 'at_hash'
ctx at_hash 400 200 'at_hash' 20
cnt oidc-libs 'openid-client|oauth4webapi|node_modules/jose|Issuer\.discover|new Issuer\(|validateIdToken|id_token_signed_response_alg'
ctx oidc-libs 200 200 'Issuer\.discover|new Issuer\(|validateIdToken' 12
cnt openid-scope '"openid"'
ctx openid-scope 200 200 '"openid"' 12
cnt id_token 'id_token'
ctx id_token 150 150 'id_token' 30
cnt dpop 'DPoP|dpop'
ctx dpop 150 150 'DPoP' 12
ctx refresh 0 1600 'P=\{grant_type:"refresh_token",refresh_token:e,client_id:s\?\?on\(\)\.CLIENT_ID' 3
ctx revoke 300 900 'async function i3r\(' 3
ctx revoke-callers 300 300 '[^a-zA-Z0-9_$]i3r\(' 12
ctx exchange 200 900 'async function c9o\(' 3
ctx token-shape 200 200 'sk-ant-o[a-z]{1,3}[0-9]{0,2}' 20
cnt token-shape 'sk-ant-o[a-z]{1,3}[0-9]{0,2}'
ctx accept-encoding 150 150 '[Aa]ccept-[Ee]ncoding' 12
cnt accept-encoding '[Aa]ccept-[Ee]ncoding'

# --- Codex at rust-v0.167.2 (its pin's tag), shallow, its commit and cited files checked.
S=$T/codex
if [ ! -d "$S/.git" ]; then
  GIT_TERMINAL_PROMPT=0 git -c core.hooksPath=/dev/null clone -q --depth 1 --branch rust-v0.167.2 https://github.com/openai/codex.git "$S" || { echo "CODEX CLONE FAILED"; exit 2; }
fi
echo "CODEX HEAD $(git -C "$S" rev-parse HEAD) (pinned commit ce21072c90d32c40af7a55b60b2fb35c84a43a6a)"
rm -f "$O"/codex-*.txt
while read -r sum path; do
  got=$(sha256sum "$S/$path" | cut -d' ' -f1)
  echo "CODEX FILE $path $got $([ "$got" = "$sum" ] && echo OK || echo MISMATCH)"
done <<'EOF'
2f66d1c033e3918bc8bfda0cdad047c4553c10ec6a33ef8f80a7f9d6c2b47c10 codex-rs/login/src/server.rs
584348a470e6d7f147962bf5d642d23859f60a85772aac1e916032d414756a90 codex-rs/login/src/auth/manager.rs
db4490daaeb89bf781d05ceeeb53f2291e4729caea2b62a635d61c98e305223e codex-rs/login/src/auth/revoke.rs
f79cdda556c137c9a244026030d604324c7c5df73b0961fc5b7a766f05f7a460 codex-rs/login/src/oauth/client.rs
075c3f83cbc2612dde7b50f118b7a3c1ef3b7641ce6acfcca99aff5dd7454012 codex-rs/login/src/device_code_auth.rs
ca59ca56d607a602bf512603eeb1f60a8f9bbe6ef9f1bbc8555de499800b32ff codex-rs/login/src/token_data.rs
ae70ac3edff091da6b0074d1eca0375c65657197d849eb46be8d16909377255d codex-rs/login/src/auth/storage.rs
7c887f1f55ea9e9514afd85ce19307c670b575a1edb323d7e805acd96cb524be codex-rs/cli/src/login.rs
5f97d4e4683f5eb11c0ba1765167e70a89093f7094c30ecad5055b2ce816679a codex-rs/model-provider/src/bearer_auth_provider.rs
4f446f23ccb25f9c160398a51717b0e86bf8643daa09b4c59da6f266823bcc82 codex-rs/login/src/auth/agent_identity.rs
EOF
cd "$S" || exit 2
{ echo "## git grep -n -i -E 'dpop' (every file)"; git grep -n -i -E 'dpop' | head -40; echo "## end"; } > "$O/codex-dpop.txt"
{ echo "## git grep -n -E 'at_hash' (every file)"; git grep -n -E 'at_hash' | head -20; echo "## end"; } > "$O/codex-at_hash.txt"
{ echo "## git grep -n -E 'obtain_api_key|token-exchange|token-type:id_token|requested_token|openai-api-key' codex-rs"
  git grep -n -E 'obtain_api_key|token-exchange|token-type:id_token|requested_token|openai-api-key' -- codex-rs | head -60; echo "## end"; } > "$O/codex-exchange.txt"
for f in codex-rs/login/src/oauth/client.rs codex-rs/login/src/auth/revoke.rs codex-rs/login/src/device_code_auth.rs codex-rs/login/src/token_data.rs; do
  { echo "## cat -n $f"; cat -n "$f"; echo "## end"; } > "$O/codex-$(basename "$f" .rs).txt"
done
{ echo "## server.rs: exchange_code_for_tokens, obtain_api_key, persist_tokens_async, and their callers"
  grep -n -E 'fn exchange_code_for_tokens|fn obtain_api_key|fn persist_tokens|obtain_api_key\(|exchange_code_for_tokens\(|persist_tokens_async\(|api_key|OPENAI_API_KEY|id_token' codex-rs/login/src/server.rs | head -80
  echo "## end"; } > "$O/codex-server-index.txt"
{ echo "## manager.rs: refresh, revoke, parse_jwt, id_token and api key"
  grep -n -E 'fn refresh|REFRESH_TOKEN_URL|REVOKE_TOKEN_URL|parse_jwt|parse_chatgpt_jwt_claims|id_token|openai_api_key|OPENAI_API_KEY|fn logout|revoke' codex-rs/login/src/auth/manager.rs | head -120
  echo "## end"; } > "$O/codex-manager-index.txt"
{ echo "## cli/src/login.rs: logout and login revoke"; grep -n -E 'revoke|logout' codex-rs/cli/src/login.rs | head -40; echo "## end"; } > "$O/codex-cli-login.txt"
} > "$log" 2>&1
echo "EXIT $?"
grep -a -E '^HOST|^CLAUDE|^CODEX|FAILED' "$log"
ls -la "$O"
