#!/bin/bash
# Step 33 measurement: the protocol each provider host selects (RFC 7301) when a client
# offers h2 and http/1.1, and when it offers http/1.1 alone.
# Usage: alpn.sh OPENSSL HOST...
set -u
ossl=$1; shift
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) $($ossl version) on $(uname -sm)"
for h in "$@"; do
  for offer in h2,http/1.1 http/1.1; do
    out=$($ossl s_client -connect "$h:443" -servername "$h" -tls1_3 -alpn "$offer" </dev/null 2>&1 | tr -d '\r')
    alpn=$(printf '%s\n' "$out" | grep -E '^ALPN protocol:' | head -1)
    echo "$h | offered $offer | ${alpn:-ALPN protocol: none}"
  done
done
