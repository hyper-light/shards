#!/bin/sh
# As musl-repro.sh (scripts/musl-ci's setup: packages, the pinned zig, tests as the
# checkout's owner, never root), then: builds the test binary of package $PKG (bin $BIN),
# runs test $TEST $N times, one at a time, and counts each outcome, keeping the first
# lines of each failure.
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
exec su -m shards -s /bin/sh /cigbin/loop-inner.sh
