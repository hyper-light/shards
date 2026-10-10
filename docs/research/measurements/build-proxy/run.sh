#!/bin/sh
# M150: what a build's proxy costs (D110, buildx's exec.proxy cap). First its certificates:
# a build's CA, a host's certificate with its TLS, and one kept, P-256 (shards) against
# RSA-2048 (BuildKit v0.33.0's newCA and certForHost), wall and thread CPU time, n each,
# interleaved. Then a request through the proxy against the same request straight to its
# server, a connection each: plain HTTP (its question answered at once, and by a real
# policy as a build's thread answers it) and HTTPS (straight TLS against a CONNECT
# tunnel). Microseconds at p50, p90, p99 and max, with this host, its OS, its load and
# the revision measured.
#
#   docs/research/measurements/build-proxy/run.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git rev-parse --short HEAD)"
echo "load: $(uptime | sed 's/.*load averages*: //')"
cargo test --release -p shards --bin shards -- --ignored --nocapture \
	build::proxy::ca::tests::certificate_costs build::proxy::server::tests::request_costs 2>&1 |
	grep -E '^(CA|host certificate|plain|HTTPS)'
# Then in microVMs: a RUN's requests to a server on this host, each on a connection of its
# own, straight through the builder's network against through the proxy, each request
# checked by the build's policy, twice in turn; the step's client times them.
cargo test --release -p shards --test build -- --ignored --nocapture proxy_request_costs_in_microvms 2>&1 |
	grep -E '^round'
echo "load after: $(uptime | sed 's/.*load averages*: //')"
