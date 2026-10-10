#!/bin/sh
# M127: what each part of a build's policy check costs on this host (n, p50, p90, p99,
# max), the memory a hostile provenance takes to read, and the stack the deepest chain
# documents need where a build reads them.
#
#   docs/research/measurements/policy-path/run.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
export SHARDS_HELPERS=skip
cargo test --release -p shards --bin shards -- --ignored \
	build::policy::signatures::tests::policy_path_costs --nocapture
# The smallest stack, in KiB to 32 KiB, on which the deepest chain is read and verified
# (an overflow aborts the process, so each size runs alone).
cargo test --release -p shards --bin shards the_deepest_chain_is --no-run
bin=$(ls -t target/release/deps/shards-* | grep -v '\.d$' | head -1)
lo=32
hi=65536
while [ $((hi - lo)) -gt 32 ]; do
	mid=$(((lo + hi) / 2 / 32 * 32))
	if SHARDS_JSON_STACK_PROBE=$mid "$bin" the_deepest_chain_is >/dev/null 2>&1; then
		hi=$mid
	else
		lo=$mid
	fi
done
echo "the deepest chain is read and verified in $hi KiB"
