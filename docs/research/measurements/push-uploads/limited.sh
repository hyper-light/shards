#!/bin/sh
# PM M488: `shards push` to a registry:2 behind nginx at its default request body limit
# (client_max_body_size 1m, ngx_http_core_module), as distribution's nginx recipe warns
# of (docs/content/recipes/nginx.md: "to avoid HTTP 413 for large image uploads"). Pushes
# nginx:alpine, which the registry at REGISTRY (a registry:2 on this host) is given by
# Docker first, then pulls it back through nginx into a fresh home, and counts nginx's
# answers by method and status.
#   limited.sh SHARDS REGISTRY_PORT [LIMIT]
set -eu
shards=$1
port=$2
limit=${3:-1m}
work=$(mktemp -d)
trap 'docker rm -f m488-nginx >/dev/null 2>&1 || true; docker rmi 127.0.0.1:$port/m488/source:1 >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
cat > "$work/nginx.conf" <<EOF
events {}
http {
  log_format m488 'request \$request_method \$status \$request_length';
  access_log /dev/stdout m488;
  client_max_body_size $limit;
  server {
    listen 5078;
    location /v2/ {
      proxy_pass http://127.0.0.1:$port;
      proxy_set_header Host \$http_host;
      proxy_request_buffering off;
    }
  }
}
EOF
docker pull -q nginx:alpine >/dev/null
docker tag nginx:alpine 127.0.0.1:$port/m488/source:1
docker push -q 127.0.0.1:$port/m488/source:1 >/dev/null
docker run -d --name m488-nginx --network host -v "$work/nginx.conf:/etc/nginx/nginx.conf:ro" nginx:alpine >/dev/null
sleep 1
export DOCKER_CONFIG="$work/docker" SHARDS_HOME="$work/home"
mkdir -p "$DOCKER_CONFIG"
repo=m488/limited-$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')
"$shards" pull -q 127.0.0.1:$port/m488/source:1
"$shards" tag 127.0.0.1:$port/m488/source:1 127.0.0.1:5078/$repo:1
t0=$(date +%s.%N)
"$shards" push 127.0.0.1:5078/$repo:1
t1=$(date +%s.%N)
echo "push took $(echo "$t1 - $t0" | bc) s"
"$shards" stop daemon >/dev/null 2>&1 || true
SHARDS_HOME="$work/back" "$shards" pull 127.0.0.1:5078/$repo:1
SHARDS_HOME="$work/back" "$shards" stop daemon >/dev/null 2>&1 || true
echo "nginx's answers to the push and the pull back, by method and status, and the bytes of each"
echo "request, head and body: count, least, most"
docker logs m488-nginx 2>/dev/null | awk '$1 == "request" {
  k = $2 " " $3; n[k]++
  if (!(k in lo) || $4 < lo[k]) lo[k] = $4
  if ($4 > hi[k]) hi[k] = $4
} END { for (k in n) print k, n[k], lo[k], hi[k] }' | sort
