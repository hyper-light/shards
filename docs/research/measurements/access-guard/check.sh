#!/bin/sh
# Checks the guest-memory access model (docs/design/architecture.md D29) with the two
# tools that know Rust's rules for concurrent accesses: ThreadSanitizer, and Miri, which
# also checks atomics of different sizes that overlap. Both run the tests where host
# threads share guest memory, among them two queues a driver laid over each other.
#
#   docs/research/measurements/access-guard/check.sh [TOOLCHAIN]
#
# TOOLCHAIN is a nightly with the rust-src and miri components (default: the one CI
# pins). negative-control.patch makes every access take a lock of its own; with it
# applied, both tools must report data races (README.md).
set -eu
toolchain=${1:-nightly-2026-09-05}
host=$(rustc +"$toolchain" -vV | sed -n 's/^host: //p')
# std is built for these tests with the sanitizer, and unwinds, as test harnesses do.
export CARGO_PROFILE_DEV_PANIC=unwind
env -u DYLD_FALLBACK_LIBRARY_PATH -u DYLD_LIBRARY_PATH \
    RUSTFLAGS=-Zsanitizer=thread TSAN_OPTIONS=halt_on_error=1 \
    cargo +"$toolchain" test -q -Zbuild-std --target "$host" --target-dir target/tsan \
    -p shards-vmm --lib -- memory:: queue::
# Miri interprets Linux x86_64 on any host: it maps only anonymous memory, and so runs the
# tests that save to no file.
env -u DYLD_FALLBACK_LIBRARY_PATH -u DYLD_LIBRARY_PATH \
    cargo +"$toolchain" miri test -q --target x86_64-unknown-linux-gnu --target-dir target/miri \
    -p shards-vmm --lib -- \
    memory::tests::index_accesses_are_aligned_only \
    memory::tests::copies_are_exact_at_every_alignment \
    memory::tests::host_threads_take_turns \
    queue::tests
