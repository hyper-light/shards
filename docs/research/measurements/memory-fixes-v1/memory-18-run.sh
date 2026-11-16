#!/usr/bin/env bash
# memory-18: green, red (as the no-cleanup mutant) and mutation checks of the refused-reply cleanup.
# Usage: run.sh <tip sha>  (cwd: this carrier)
set -u
tip=$1
here=$(pwd)
work=$HOME/g121-tmp/m18
mkdir -p "$work"
export CARGO_TARGET_DIR=$work/target CARGO_INCREMENTAL=0
if [ ! -d "$work/src/.git" ]; then
  for i in 1 2 3 4 5 6; do
    git clone -q --filter=blob:none https://github.com/hyper-light/shards.git "$work/src" && break
    rm -rf "$work/src"; sleep 30
  done
fi
cd "$work/src" || exit 2
for i in 1 2 3 4 5 6; do git fetch -q origin main && break; sleep 30; done
git fetch -q "$here/change.bundle" "$tip" || exit 2
git checkout -q --detach "$tip" || exit 2
git log --oneline -1
uname -a; rustc --version
nice="nice -n 10"
fs=crates/vmm/src/devices/virtio/fs
t() { perl -e 'alarm shift; exec @ARGV' 900 $nice cargo test -q -p shards-vmm --lib -- "$@" 2>&1 | grep -E "^test |test result|panicked|left:|right:|error" | grep -v "\.\.\. ok$" | head -40; return "${PIPESTATUS[0]}"; }

if [ -z "${ONLY:-}" ]; then
echo "=== green: devices::virtio::fs"
$nice cargo test -q -p shards-vmm --lib devices::virtio::fs 2>&1 | grep -E "test result|FAILED|panicked" | head -20
echo "green exit ${PIPESTATUS[0]}"
echo "=== green: memory-18 tests by name"
$nice cargo test -p shards-vmm --lib -- refused_lookups refused_creates replies_that_do_not_fit refusing_an_open refused_directory a_refused_lookup_of a_broken_cleanup 2>&1 | grep -E "^test |test result"
echo "=== clippy"
$nice cargo clippy -q -p shards-vmm --all-targets -- -D warnings 2>&1 | tail -20
echo "clippy exit ${PIPESTATUS[0]}"
fi

mutate() { # name file old new filters...
  local name=$1 file=$2 old=$3 new=$4; shift 4
  if [ -n "${ONLY:-}" ] && [ "$ONLY" != "$name" ]; then return; fi
  python3 - "$file" "$old" "$new" <<'EOF' || { echo "MUTANT $name: pattern not found"; return; }
import sys
p, old, new = sys.argv[1:]
s = open(p).read()
if s.count(old) != 1: sys.exit(1)
open(p, 'w').write(s.replace(old, new))
EOF
  touch "$file"
  echo "=== mutant $name"
  t "$@"; local rc=$?
  if [ "$rc" -ne 0 ]; then echo "MUTANT $name: KILLED (exit $rc)"; else echo "MUTANT $name: SURVIVED"; fi
  git checkout -q -- "$file"; touch "$file"
}
mutate red-no-cleanup $fs/mod.rs '.and_then(|conn| release_reply(conn, req, reply));' '.map(|_| ());' \
  refused_lookups refused_creates replies_that_do_not_fit refused_directory a_refused_lookup_of
mutate no-forget $fs/mod.rs 'if node != 0 && done.is_ok() {' 'if false && node != 0 && done.is_ok() {' \
  refused_lookups refused_creates refused_directory a_refused_lookup_of
mutate forget-two $fs/mod.rs "done = send(proto::op::FORGET, node, &1u64.to_le_bytes());" "done = send(proto::op::FORGET, node, &2u64.to_le_bytes());" \
  a_refused_lookup_of
mutate no-release $fs/mod.rs 'if let Some((release, node, at)) = handle {' 'if let Some((release, node, at)) = handle.filter(|_| false) {' \
  refused_creates refused_directory
mutate scatter-short-ok $fs/mod.rs 'if rest.is_empty() { Ok(written) } else { Err(()) }' 'Ok(written)' \
  replies_that_do_not_fit
mutate scatter-no-refuse $fs/mod.rs '            slot.refuse(req, reply);
            scatter(chain, mem, reply).unwrap_or(0)' '            if let Some(e) = proto::unreachable(req) { reply.clear(); reply.extend_from_slice(&e); }
            scatter(chain, mem, reply).unwrap_or(0)' \
  replies_that_do_not_fit
mutate keep-broken-conn $fs/mod.rs '                *conn = None;
            }
        }' '            }
        }' \
  a_broken_cleanup
mutate refused-release-no-forget $fs/mod.rs 'if !matches!(released, Ok(()) | Err(Unreleased::Refused { .. })) {' 'if released.is_err() {' \
  a_broken_cleanup
mutate refused-release-detaches $fs/mod.rs 'Err(why @ Unreleased::Refused { .. }) => {' 'Err(why @ Unreleased::Refused { .. }) if false => {' \
  a_broken_cleanup
git status --short
