#!/bin/sh
# CI's musl job's setup (scripts/musl-ci): Alpine's packages and the pinned zig as root, the
# checks as the checkout's owner, never root, under Docker's default seccomp profile. Runs
# checks.sh from /work for $TARGET.
#
#   docker run --rm -v "$PWD:/work" -w /work -e TARGET IMAGE \
#     sh docs/research/measurements/musl-opath-v1/harness/container-musl.sh
set -eu
tries=0
until apk add --no-cache build-base curl xz >/dev/null; do
  tries=$((tries + 1)); [ "$tries" -ge 8 ] && exit 1; sleep $((1 << (tries - 1)))
done
zig=$(RUNNER_TEMP=/opt GITHUB_PATH= scripts/install-zig)
chmod -R a+rX "$zig"
export PATH="$zig:$PATH"
uid=$(stat -c %u /work)
[ "$uid" != 0 ] || { echo "/work is root's; the checks run as its owner" >&2; exit 1; }
adduser -D -u "$uid" -h /home/shards shards
grep -E "^(Seccomp|NoNewPrivs|CapEff):" /proc/self/status
exec su -m shards -s /bin/sh -c '
  export HOME=/home/shards CARGO_HOME=/home/shards/.cargo
  grep -E "^(Uid|CapEff):" /proc/self/status
  exec sh docs/research/measurements/musl-opath-v1/harness/checks.sh
'
