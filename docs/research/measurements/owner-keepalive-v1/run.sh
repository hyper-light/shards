#!/usr/bin/env bash
# owner-keepalive-v1 (finding H1): the red, green and mutation light tests of the managed
# owner-output transport's keep-alive, then shards-remote's gates, on a checkout of the commit
# that carries it (or a later one).
#
#   run.sh TREE LOGS
#
#   red     the base's transport: the fix taken back, m1 and m4 together (no keep-alive, no
#           millisecond floor), which leaves the base's `fn transport` code exactly
#   green   TREE as it is
#   m1..m5  one guard broken each (README.md, "Mutations")
#   gates   cargo fmt --all --check; cargo clippy -p shards-remote --all-targets -D warnings;
#           cargo test -p shards-remote (unit and loopback)
#
# Each phase writes LOGS/<phase>.log, which ends "exit N"; TREE is restored after each.
# CARGO_TARGET_DIR and TMPDIR are the caller's; run it in the runner's admitted envelope,
# e.g. systemd-run --user -p MemoryMax=16G, as the README records.
set -u
tree=$(cd "${1:?TREE}" && pwd)
logs=$(mkdir -p "${2:?LOGS}" && cd "$2" && pwd)
cd "$tree" || exit 2
managed=crates/remote/src/managed.rs
tls=crates/remote/src/tls.rs
pristine=$(mktemp -d)
cp "$managed" "$pristine/managed.rs"
cp "$tls" "$pristine/tls.rs"

stamp() { date -u +%FT%TZ; }
head_of() {
  echo "== $(stamp) $1"
  uname -srm
  rustc -V
  nproc
  uptime
  sha256sum "$managed" "$tls"
}
restore() {
  cp "$pristine/managed.rs" "$managed"
  cp "$pristine/tls.rs" "$tls"
  # A restored file keeps its old mtime; cargo would keep the mutant's build.
  touch "$managed" "$tls"
}
# Replaces the one occurrence of $2 in $1 by $3; fails where it is not found exactly once.
mutate() {
  python3 -I - "$1" "$2" "$3" <<'PY' && touch "$1"
import pathlib, sys
path, old, new = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = path.read_text()
if text.count(old) != 1:
    sys.exit(f"mutation target found {text.count(old)} times in {path}")
path.write_text(text.replace(old, new))
PY
}
managed_tests() {
  { head_of "$1: cargo test -p shards-remote --lib -- managed::tests::"
    cargo test --locked -j4 -p shards-remote --lib -- managed::tests:: --nocapture
    echo "exit $? $(stamp)"; } > "$logs/$1.log" 2>&1
}

no_keep_alive=($managed $'        ))\n        .keep_alive_interval(Some(tls::keep_alive(admitted.patience)));' $'        ));')
fixed_interval=($managed '.keep_alive_interval(Some(tls::keep_alive(admitted.patience)));'
  '.keep_alive_interval(Some(tls::keep_alive(tls::QUIET)));')
no_idle_bound=($managed $'.max_idle_timeout(Some(\n            IdleTimeout::try_from(admitted.patience)\n                .map_err(|_| Error::Admission(AdmissionError::Patience))?,\n        ))'
  '.max_idle_timeout(None)')
no_floor=($managed $'    if admitted.patience.as_millis() == 0 {\n        return Err(Error::Admission(AdmissionError::Patience));\n    }\n' '')
half_interval=($tls $'    bound / 4\n' $'    bound / 2\n')

mutate "${no_keep_alive[@]}" && mutate "${no_floor[@]}" && managed_tests red
restore
managed_tests green
mutate "${no_keep_alive[@]}" && managed_tests m1-no-keep-alive
restore
mutate "${fixed_interval[@]}" && managed_tests m2-fixed-interval
restore
mutate "${no_idle_bound[@]}" && managed_tests m3-no-idle-bound
restore
mutate "${no_floor[@]}" && managed_tests m4-no-millisecond-floor
restore
mutate "${half_interval[@]}" && managed_tests m5-half-interval
restore
{ head_of "fmt: cargo fmt --all --check"
  cargo fmt --all --check
  echo "exit $? $(stamp)"; } > "$logs/fmt.log" 2>&1
{ head_of "clippy: cargo clippy -p shards-remote --all-targets -- -D warnings"
  cargo clippy --locked -j4 -p shards-remote --all-targets -- -D warnings
  echo "exit $? $(stamp)"; } > "$logs/clippy.log" 2>&1
{ head_of "tests: cargo test -p shards-remote (unit and loopback)"
  cargo test --locked -j4 -p shards-remote -- --nocapture
  echo "exit $? $(stamp)"; } > "$logs/tests.log" 2>&1
rm -r "$pristine"
echo "done $(stamp)" > "$logs/done"
