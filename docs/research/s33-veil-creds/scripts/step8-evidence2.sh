#!/bin/bash
# Step 8's evidence, second pass: the exact line ranges cited in D242 and STEP8.md, read from
# the copies evidence.sh held to their pins (never run).
#   bash evidence2.sh
set -u
export LC_ALL=C
T=$HOME/g121-tmp/s33-signin/evidence; O=$T/out; S=$T/codex; B=$T/claude
log=$T/evidence2.log
{
echo "HOST $(hostname) $(uname -sm) $(date -u +%FT%TZ)"
[ "$(sha256sum "$B" | cut -d' ' -f1)" = 86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada ] || { echo "CLAUDE SHA MISMATCH"; exit 2; }
[ "$(git -C "$S" rev-parse HEAD)" = ce21072c90d32c40af7a55b60b2fb35c84a43a6a ] || { echo "CODEX HEAD MISMATCH"; exit 2; }
cd "$S" || exit 2
show() { echo "## sed -n '$2,$3p' $1"; sed -n "$2,$3p" "$1" | cat -A | sed 's/\$$//' | cut -c1-180; echo "## end"; }
{ show codex-rs/login/src/server.rs 415 470
  show codex-rs/login/src/server.rs 696 760
  show codex-rs/login/src/server.rs 835 880
  show codex-rs/login/src/server.rs 1036 1075; } > "$O/codex2-server.txt"
{ show codex-rs/login/src/auth/manager.rs 1725 1770
  show codex-rs/login/src/auth/manager.rs 1850 1900
  show codex-rs/login/src/auth/manager.rs 3525 3547
  show codex-rs/login/src/auth/manager.rs 3612 3670; } > "$O/codex2-manager.txt"
{ echo "## git grep -n -w -i dpop -- codex-rs"; git grep -n -w -i dpop -- codex-rs | head -20; echo "## end"
  echo "## git grep -n -E 'chatgpt\.com/backend-api|DEFAULT_CHATGPT_BASE_URL' -- codex-rs/model-provider codex-rs/login codex-rs/core/src | head"
  git grep -n -E 'chatgpt\.com/backend-api|DEFAULT_CHATGPT_BASE_URL' -- codex-rs/model-provider codex-rs/login codex-rs/core/src | head -12; echo "## end"
  echo "## git grep -n -E 'login_server_e2e|old mechanism' codex-rs/login/tests/suite/login_server_e2e.rs"
  sed -n '222,236p' codex-rs/login/tests/suite/login_server_e2e.rs; echo "## end"; } > "$O/codex2-misc.txt"
{ echo "## grep -a -o -b -E '.{0,200}API_KEY_URL.{0,500}' claude (payload)"
  grep -a -o -b -E '.{0,200}on\(\)\.API_KEY_URL.{0,500}' "$B" | tr -c '[:print:]\n' '.' | head -4; echo "## end"
  echo "## grep -a -o -b -E '.{0,300}raw_key.{0,200}' claude"
  grep -a -o -b -E '.{0,300}raw_key.{0,200}' "$B" | tr -c '[:print:]\n' '.' | head -4; echo "## end"
  echo "## grep -a -o -b -E 'function deo\(.{0,300}|function Sxt\(.{0,400}' claude"
  grep -a -o -b -E 'function deo\(.{0,300}|function Sxt\(\)\{.{0,400}' "$B" | tr -c '[:print:]\n' '.' | head -4; echo "## end"; } > "$O/claude2.txt"
} > "$log" 2>&1
echo "EXIT $?"
grep -a -E '^HOST|MISMATCH' "$log"
