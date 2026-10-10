#!/bin/sh
# M150's BuildKit part: what BuildKit v0.33.0's own proxy certificates cost (newCA and
# certForHost, RSA-2048), measured as certificate_costs measures shards' (run.sh): built
# with the Go release BuildKit's go.mod names, for Linux on this host's architecture, and
# run in a Linux container (CONTAINER, shards-dind by default) in a directory of its own,
# removed after.
#
#   docs/research/measurements/build-proxy/buildkit.sh [BUILDKIT_CHECKOUT]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
commit=dddd5621af04ea57823085c93a063383f71d3173 # v0.33.0
container=${CONTAINER:-shards-dind}
buildkit=${1:-}
work=$(mktemp -d)
dir=/root/shards-proxy-costs
cleanup() {
	rm -rf "$work"
	[ -n "${buildkit_tmp:-}" ] && rm -rf "$buildkit_tmp"
	[ -n "${test_file:-}" ] && rm -f "$test_file"
	docker exec "$container" rm -rf "$dir" >/dev/null 2>&1 || true
}
trap cleanup EXIT
if [ -z "$buildkit" ]; then
	buildkit_tmp=$(mktemp -d)
	buildkit=$buildkit_tmp
	git -C "$buildkit" init --quiet
	git -C "$buildkit" fetch --quiet --depth 1 https://github.com/moby/buildkit "$commit"
	git -C "$buildkit" checkout --quiet FETCH_HEAD
fi
if [ "$(git -C "$buildkit" rev-parse HEAD)" != "$commit" ]; then
	echo "$buildkit is not moby/buildkit $commit" >&2
	exit 1
fi
test_file="$buildkit/util/network/proxyprovider/zz_shards_costs_test.go"
cp "$here/buildkit_costs_test.go" "$test_file"
go=$(sed -n 's/^go //p' "$buildkit/go.mod")
case $(uname -m) in
arm64 | aarch64) arch=arm64 ;;
x86_64 | amd64) arch=amd64 ;;
*)
	echo "unknown architecture $(uname -m)" >&2
	exit 1
	;;
esac
(cd "$buildkit" && GOTOOLCHAIN="go$go" GOFLAGS=-mod=vendor GOOS=linux GOARCH="$arch" CGO_ENABLED=0 \
	go test -c -o "$work/costs.test" ./util/network/proxyprovider/)
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(docker exec "$container" uname -sr) in $container"
echo "BuildKit: v0.33.0 ($commit), go$go"
echo "load: $(uptime | sed 's/.*load averages*: //')"
docker exec "$container" mkdir -p "$dir"
docker cp "$work/costs.test" "$container:$dir/costs.test"
docker exec "$container" "$dir/costs.test" -test.count=1 -test.run '^TestShardsCertificateCosts$' | grep -E '^(CA|host)'
echo "load after: $(uptime | sed 's/.*load averages*: //')"
