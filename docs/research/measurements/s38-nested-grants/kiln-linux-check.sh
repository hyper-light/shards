#!/bin/bash
# Retries D234's two checks at c026df6a2 through kiln run on kiln's Linux runners until one
# answers; every attempt logged. Files go by git archive on stdin: no sync, no GitHub fetch.
W=/Users/adalundhe/Projects/shards/.claude/worktrees/s38-nested-grants
C=c026df6a2dc1e9b4430ca2812e5cff03b655cc53
LOG=/tmp/s38-kiln-check/attempts-clean.log
FILES="AGENTS.md CLAUDE.md docs/development-rules.md docs/development-evidence.md resources/supported-targets.txt .github/workflows/ci.yml .github/workflows/release.yml scripts/lint scripts/check-development-policy Cargo.toml docs/design/architecture.md crates/abi/src/lib.rs crates/abi/src/run.rs crates/ipc/src/lib.rs docs/research/measurements/s38-nested-grants/literal-audit.py"
REMOTE='d=$(mktemp -d) && tar -x -C "$d" && cd "$d" && echo "commit='$C'" && python3 scripts/check-development-policy; echo "policy_exit=$?"; python3 docs/research/measurements/s38-nested-grants/literal-audit.py | tail -4; echo "audit_exit=${PIPESTATUS[0]}"; uname -sm; hostname; cd / && rm -rf "$d"'
for round in $(seq 1 60); do
  for r in 00aad8daff2fa3c9daa65d53c425d450 451397fcc585f7f7cb9aaf00e42064bb 8fadc1ec213332e16cb0d915a3fb913f; do
    out=$(git -C "$W" archive "$C" $FILES | timeout 300 /Users/adalundhe/.local/bin/kiln run -q --on "$r" --no-sync --stdin -- "$REMOTE" 2>&1)
    rc=$?
    printf '%s round %s runner %s rc %s\n%s\n---\n' "$(date -u +%FT%TZ)" "$round" "$r" "$rc" "$out" >> "$LOG"
    if [ "$rc" -eq 0 ] && echo "$out" | grep -q "policy_exit=0" && echo "$out" | grep -q "audit_exit=0" && echo "$out" | grep -q "^Linux"; then
      echo "SUCCESS on $r at round $round"; echo "$out"; exit 0
    fi
  done
  sleep 60
done
echo "no Linux runner answered in 60 rounds; see $LOG"; exit 1
