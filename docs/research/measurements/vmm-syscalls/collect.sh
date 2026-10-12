#!/bin/sh
# The syscalls shards-vm and shards-net make, by thread, while the VM E2E tests run
# (parse.py): each test binary under `strace -f`, on a Linux host with /dev/kvm, and of
# `build`'s, those whose builder has a network or a proxy. Timeouts may fail tests that
# strace slows; what matters is the paths they take.
#
#   docs/research/measurements/vmm-syscalls/collect.sh [OUT_DIR]
set -eu
out=${1:-target/vmm-syscalls}
mkdir -p "$out"
root=$(cd "$(dirname "$0")/../../../.." && pwd)
cd "$root"
# A collection whose tests did not build measured nothing: it fails, saying why.
if ! cargo test --locked --release -p shards --no-run --tests >"$out/build-tests.log" 2>&1; then
  echo "collect: the tests did not build ($out/build-tests.log, its end:)" >&2
  tail -n 40 "$out/build-tests.log" >&2
  exit 1
fi
for test in boot block pmem snapshot vsock lifecycle layers erofs run warm daemon containers tty images build; do
  # Of `build`'s, the steps that reach the network, directly or through the build's proxy.
  filters=""
  if [ "$test" = build ]; then filters="network proxy"; fi
  bin=$(cargo test --locked --release -p shards --test "$test" --no-run --message-format=json 2>/dev/null |
    python3 -c 'import json,sys
for l in sys.stdin:
    m=json.loads(l)
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m["target"]["kind"]==["test"]: print(m["executable"])' | tail -1)
  if [ -z "$bin" ]; then
    echo "collect: no test binary $test" >&2
    exit 1
  fi
  echo "== $test"
  # shellcheck disable=SC2086 # the filters, each its own argument
  if ! SHARDS_REQUIRE_VMS=1 strace -f -qq -o "$out/$test.trace" -s 64 "$bin" --test-threads 4 $filters >"$out/$test.log" 2>&1; then
    echo "   (some tests failed under strace: $out/$test.log, its end:)"
    tail -n 15 "$out/$test.log" | sed 's/^/     /'
  fi
done
echo "# shards-vm"
python3 "$(dirname "$0")/parse.py" "$out"/*.trace
echo "# shards-net"
python3 "$(dirname "$0")/parse.py" --program shards-net "$out"/*.trace
# What it makes once confined: after Landlock's last step (confine.rs, at_start), where
# its filter goes.
echo "# shards-net, once confined"
python3 "$(dirname "$0")/parse.py" --program shards-net --after landlock_restrict_self "$out"/*.trace
# What a filter refused, as strace reports the SIGSYS it raised.
echo "## refused"
grep -h -o 'SIGSYS {[^}]*si_syscall=[^,}]*' "$out"/*.trace | grep -o 'si_syscall=[^,}]*' | sort | uniq -c || echo "none"
