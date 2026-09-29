#!/bin/sh
# M19 (docs/research/platform-measurements.md): AWS-LC's first random fill in a new
# process, with its CPU Jitter entropy source (the default) and without it
# (AWS_LC_SYS_NO_JITTER_ENTROPY=1). Each variant is built in its own target directory,
# then run as N fresh processes, interleaved. Prints: variant first_us second_us
#
#   docs/research/measurements/aws-lc-entropy/run.sh [N]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
n=${1:-50}
cd "$here"
# Both explicit: the repository's .cargo/config.toml turns jitter off unless the
# environment already says otherwise.
AWS_LC_SYS_NO_JITTER_ENTROPY=0 cargo build --release --quiet --target-dir target/jitter
AWS_LC_SYS_NO_JITTER_ENTROPY=1 cargo build --release --quiet --target-dir target/os
i=0
while [ $i -lt "$n" ]; do
	echo "jitter $(target/jitter/release/aws-lc-entropy)"
	echo "os $(target/os/release/aws-lc-entropy)"
	i=$((i + 1))
done
