#!/bin/bash
# Round 3 of D234 step 3's light checks, on adacachy, of commit $1.
W=/Users/adalundhe/Projects/shards/.claude/worktrees/s38-daemon
REV=$1
stage() { # stage [args]
  echo "#### stage $* at $REV ($(date -u +%FT%TZ))"
  git -C "$W" archive "$REV" | ssh -o BatchMode=yes -o ServerAliveInterval=60 adalundhe@192.168.1.77 "bash s38-daemon-remote.sh $*" 2>&1 | grep -v '^skipping non-regular'
  echo "#### stage $1 exit ${PIPESTATUS[1]}"
}
stage tests
stage clippy
stage fmt
stage audit
stage mutants
# Restored: the named tests pass again, each after its mutant was put back.
stage only cargo test -p shards --bin shards -j4 nesting
stage only cargo test -p shards --bin shards -j4 readiness
stage only cargo test -p shards --bin shards -j4 agentfile::tests
stage only cargo test -p shards --bin shards -j4 setup::tests
stage only cargo test -p shards-vm-process --bin shards-vm -j4 readiness
stage release-tests
