#!/bin/sh
# M128: what reading an image's config costs (shards_image::oci::parse_config, which every
# pull, load, listing and run takes): Go's reader and the serde reader it replaced, n
# readings each, interleaved, at p50, p90, p99 and max in microseconds, for a build-sized
# config, one with a long history, and one near MAX_CONFIG; with this host, its OS, its
# load and the revision measured.
#
#   docs/research/measurements/image-config/run.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git rev-parse --short HEAD)"
echo "load: $(uptime | sed 's/.*load averages*: //')"
cargo test --release -p shards-image --lib -- --ignored oci::tests::parse_config_costs --nocapture 2>&1 |
	grep -E '^(build-sized|long history|near MAX_CONFIG):'
