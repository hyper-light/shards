#!/bin/bash
# Step 33 measurement: does each provider host accept a TLS 1.3 handshake whose client
# offers TLS_AES_256_GCM_SHA384 alone? (The injector's upstream side, AES-256-GCM only.)
# Usage: tls13-aes256.sh OPENSSL HOST...
set -u
ossl=$1; shift
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) $($ossl version) on $(uname -sm)"
for h in "$@"; do
  out=$($ossl s_client -connect "$h:443" -servername "$h" -tls1_3 -ciphersuites TLS_AES_256_GCM_SHA384 -brief </dev/null 2>&1 | tr -d '\r')
  suite=$(printf '%s\n' "$out" | grep -E '^Ciphersuite:' | head -1)
  proto=$(printf '%s\n' "$out" | grep -E '^Protocol version:' | head -1)
  group=$(printf '%s\n' "$out" | grep -E '^(Negotiated TLS1.3 group|Server Temp Key|Peer Temp Key):' | head -1)
  verify=$(printf '%s\n' "$out" | grep -E '^Verification:' | head -1)
  echo "$h | ${proto:-no handshake} | ${suite:-none} | ${group:-?} | ${verify:-?}"
done
