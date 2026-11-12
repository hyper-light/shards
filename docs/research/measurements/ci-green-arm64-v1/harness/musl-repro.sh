#!/bin/sh
# Mirrors scripts/musl-ci (packages, pinned zig, tests as the checkout owner, never root),
# but runs only the cargo test arguments given: TEST_ARGS.
set -eu
tries=0
until apk add --no-cache build-base curl xz >/dev/null; do
  tries=$((tries + 1)); [ "$tries" -ge 8 ] && exit 1; sleep $((1 << (tries - 1)))
done
zig=$(RUNNER_TEMP=/opt GITHUB_PATH= scripts/install-zig)
chmod -R a+rX "$zig"
export PATH="$zig:$PATH"
uid=$(stat -c %u /work)
adduser -D -u "$uid" -h /home/shards shards
grep -E "^(Seccomp|Seccomp_filters|NoNewPrivs|CapEff):" /proc/self/status
exec su -m shards -s /bin/sh -c "
  set -eu
  export HOME=/home/shards CARGO_HOME=/cargo
  grep -E \"^(Seccomp|CapEff):\" /proc/self/status
  cargo test --locked --profile test-release --target \"\$TARGET\" $TEST_ARGS
"
