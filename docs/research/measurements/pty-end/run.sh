#!/bin/sh
# PM M246: what a late reader of a pty's controller reads of a command's last output, as
# the daemon's terminals for clients' commands keep them (probe.c): N trials of each
# writer, strategy, reader delay and size; each line one trial. A leader's `last` waits
# for nothing, `kept` for its reader, `gone` for the kept writer let go.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${TMPDIR:-/tmp}/pty-end-probe
libs=-lpthread
# openpty(3) is libutil's on Linux (glibc before 2.34, musl), libSystem's on macOS.
[ "$(uname -s)" = Linux ] && libs="$libs -lutil"
cc -O2 -Wall -o "$out" "$here/probe.c" $libs
n=${N:-5}
uname -srm
for writer in plain leader; do
  for strategy in last kept; do
    for delay in 0 100 300 600 1000 2000; do
      for bytes in 64 500 4000; do
        i=0
        while [ "$i" -lt "$n" ]; do
          "$out" "$writer" "$strategy" "$delay" "$bytes"
          i=$((i + 1))
        done
      done
    done
  done
  for strategy in gone gone-flushed; do
    for delay in 0 1000; do
      i=0
      while [ "$i" -lt "$n" ]; do
        "$out" "$writer" "$strategy" "$delay" 500
        i=$((i + 1))
      done
    done
  done
done
