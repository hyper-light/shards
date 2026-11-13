#!/bin/bash
# PM M537's measurements on this host, from a checkout's root: the crossovers
# (digits_crossover.rs), the allocations against the bound (tests/text_memory.rs), and
# whole calls' time and peak memory (text/run.sh), built for release. CARGO_TARGET_DIR
# may name a target elsewhere (a full disk's host: its tmpfs).
#
#   bash docs/research/measurements/rego-bignum/measure.sh > LOG 2>&1
set -u
echo "host: $(uname -srm), $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), $(nproc 2>/dev/null || sysctl -n hw.ncpu) cpus, $(git rev-parse --short HEAD 2>/dev/null), load: $(uptime | sed 's/.*load/load/')"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
mkdir -p "$CARGO_TARGET_DIR"
(cd crates/rego &&
	cargo test --release --lib digits::crossover -- --ignored --nocapture --test-threads 1 2>&1 | grep -v '^\s*\(Compiling\|Finished\|Running\)' &&
	cargo test --release --test text_memory -- --nocapture 2>&1 | grep -E 'bits,|test result')
sh docs/research/measurements/rego-bignum/text/run.sh 2>&1 | grep -v '^\s*\(Compiling\|Finished\|Locking\|Updating\|Downloaded\|Downloading\)'
echo "load after: $(uptime | sed 's/.*load/load/')"
