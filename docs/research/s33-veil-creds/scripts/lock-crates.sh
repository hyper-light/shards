#!/bin/bash
# Step 33 measurement: which TLS, X.509 and HTTP crates Shards' Cargo.lock holds.
# Usage: lock-crates.sh WORKTREE
set -u
w=$1
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) Cargo.lock of $(git -C "$w" rev-parse HEAD)"
for c in rustls rustls-webpki rustls-platform-verifier rustls-native-certs webpki-roots aws-lc-rs rcgen x509-parser httparse h2 hpack hyper http tokio tungstenite; do
  v=$(awk -v n="$c" '$0=="[[package]]"{getline; if ($0=="name = \""n"\"") {getline; print $3}}' "$w/Cargo.lock" | tr -d '"' | tr '\n' ' ' | sed 's/ $//')
  echo "$c: ${v:-absent}"
done
