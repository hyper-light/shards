#!/bin/sh
# M126: OPA's time on deeply nested policies, and the stack shards' policy thread needs
# for the deepest policy OPA parses and the deepest JSON load_json reads.
#
#   docs/research/measurements/rego-depth/run.sh BUILDX_V0.37.1_CHECKOUT
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
bx=$1
mkdir -p "$bx/cmd/zz_rego_depth"
cp "$here/opa_depth.go" "$bx/cmd/zz_rego_depth/main.go"
trap 'rm -rf "$bx/cmd/zz_rego_depth"' EXIT
(cd "$bx" && go build -mod=vendor -o "$here/.opa_depth" ./cmd/zz_rego_depth)
for n in 1000 2000 4000 8000 33333; do
	echo "== OPA, depth $n"
	"$here/.opa_depth" "$n"
done
rm -f "$here/.opa_depth"
# shards: the smallest stack, in MiB, on which the_deepest_policy_runs_on_the_policy_thread
# passes (an overflow aborts the process, so each size runs alone).
cd "$root"
cargo test --release -p shards --bin shards policy::tests::the_deepest --no-run
bin=$(ls -t target/release/deps/shards-* | grep -v '\.d$' | head -1)
lo=1
hi=1024
while [ $((hi - lo)) -gt 1 ]; do
	mid=$(((lo + hi) / 2))
	if SHARDS_STACK_PROBE=$((mid << 20)) "$bin" policy::tests::the_deepest >/dev/null 2>&1; then
		hi=$mid
	else
		lo=$mid
	fi
done
echo "shards' policy thread needs $hi MiB"
