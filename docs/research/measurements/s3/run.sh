#!/bin/sh
# M124: the S3 cache backend's requests against a real S3 (versitygw, which checks
# SigV4), run inside the shards-dind container and reached on loopback through
# tunnel.py. Usage: docs/research/measurements/s3/run.sh (from the repository root).
set -eu
image=versity/versitygw@sha256:30292fc2eeacc67a36993b01f7a7a5e3361a19cced0e80c1d71cfa2a4b0a2499
name=shards-s3-probe-$$
docker exec shards-dind docker run -d --name "$name" --entrypoint sh "$image" -c \
  'mkdir -p /tmp/data/cache && exec versitygw --access PROBEKEY --secret probesecret12345 --region us-east-1 posix /tmp/data' >/dev/null
trap 'docker exec shards-dind docker rm -f "$name" >/dev/null; kill "$tunnel" 2>/dev/null' EXIT
ip=$(docker exec shards-dind docker inspect "$name" --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
sleep 2
ports=$(mktemp)
python3 "$(dirname "$0")/tunnel.py" "$ip" 7070 >"$ports" &
tunnel=$!
until [ -s "$ports" ]; do sleep 0.1; done
SHARDS_TEST_S3="http://127.0.0.1:$(cat "$ports")" SHARDS_TEST_S3_KEY=PROBEKEY SHARDS_TEST_S3_SECRET=probesecret12345 \
  cargo test --release -p shards --bin shards s3_requests_are_a_real_s3s -- --nocapture
