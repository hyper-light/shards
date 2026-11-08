#!/bin/sh
# M125: the azblob cache backend's requests against Azurite, Microsoft's Blob Storage
# emulator, which checks shared-key signatures, run inside the shards-dind container and
# reached on loopback through ../s3/tunnel.py. Usage, from the repository root:
#   docs/research/measurements/azblob/run.sh
set -eu
image=mcr.microsoft.com/azure-storage/azurite@sha256:830430c1da1a2d537e08f3e6764dd1f5ae00cf0346bcaf625b968ec3f0971fd5
name=shards-azblob-probe-$$
docker exec shards-dind docker run -d --name "$name" "$image" azurite-blob --blobHost 0.0.0.0 >/dev/null
trap 'docker exec shards-dind docker rm -f "$name" >/dev/null; kill "$tunnel" 2>/dev/null' EXIT
ip=$(docker exec shards-dind docker inspect "$name" --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
sleep 2
ports=$(mktemp)
python3 "$(dirname "$0")/../s3/tunnel.py" "$ip" 10000 >"$ports" &
tunnel=$!
until [ -s "$ports" ]; do sleep 0.1; done
SHARDS_TEST_AZBLOB="http://127.0.0.1:$(cat "$ports")/devstoreaccount1" \
  cargo test --release -p shards --bin shards azblob_requests_are_blob_storages -- --nocapture
