#!/bin/sh
# PM M492: builds the harness, then, for a session of logman's default buffers and one of
# 128 KB buffers, each a file-mode session of a provider GUID of its own, the most units
# EventWriteString takes. Run elevated (an ETW session is an administrator's or
# Performance Log Users'), from this directory, on Windows (Git Bash). Needs cargo.
set -eu
GUID=6d2a5c1e-4b7f-4e0a-9c3d-2f8e1a7b5c90
cargo build --release --locked
bin=${CARGO_TARGET_DIR:-target}/release/etw-event-size.exe
etl=$(cygpath -w "${TMPDIR:-/tmp}")\\etw-event-size
for bs in default 128; do
  name=etw-event-size-$bs
  if [ "$bs" = default ]; then
    logman start "$name" -p "{$GUID}" -o "$etl-$bs.etl" -ets >/dev/null
  else
    logman start "$name" -p "{$GUID}" -o "$etl-$bs.etl" -bs "$bs" -ets >/dev/null
  fi
  echo "session $name:"; logman query "$name" -ets | grep -i "buffer size" || true
  "$bin" "$GUID" || true
  logman stop "$name" -ets >/dev/null
  rm -f "$(cygpath -u "$etl-$bs.etl")"
done
