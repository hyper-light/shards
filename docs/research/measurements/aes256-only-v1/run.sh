#!/usr/bin/env bash
# The light checks of aes256-only-v1, from the repository root on Linux x86_64, each stage's
# raw output on stdout and in $TMPDIR/aes256-only-v1/STAGE.log, whose SHA-256 ends it:
#   red        the cipher tests on the base's providers (mutations base-remote, base-server)
#   green      shards-remote's whole suite (unit and tests/loopback.rs), shards-server's unit tests
#   mutations  ChaCha20-Poly1305, then AES-128-GCM, allowed again in each provider
#   gates      policy, fmt, clippy -D warnings (x86_64 gnu; shards-server on both musl targets)
# kiln-a094a12d ran it as: systemd-run --user --scope -q -p MemoryMax=24G -- bash run.sh STAGE,
# with CARGO_BUILD_JOBS=4, TMPDIR=$HOME/g121-tmp and zig 0.16.0 first on PATH.
set -u
here=docs/research/measurements/aes256-only-v1
logs="${TMPDIR:-/tmp}/aes256-only-v1"
mkdir -p "$logs"
quiet() { grep -v -e '^warning: aws-lc-sys' -e '^ *Compiling ' -e '^ *Checking '; }
stage() {
  echo "HEAD=$(git rev-parse HEAD) host=$(hostname) kernel=$(uname -r) $(rustc --version) zig=$(zig version)"
  git status --short
  # kiln's sync gives each file the Mac's mtime, older than a mutant built here, and cargo then
  # kept the mutant's build (02-INVALID-green-stale-mutant.log): touched, so it builds them anew.
  touch crates/remote/src/*.rs crates/server/src/*.rs
  case "$1" in
    red) "$here/mutations.sh" base-remote base-server ;;
    green)
      cargo test -p shards-remote 2>&1 | quiet
      echo "remote_test_exit=${PIPESTATUS[0]}"
      cargo test -p shards-server --target x86_64-unknown-linux-gnu 2>&1 | quiet
      echo "server_test_exit=${PIPESTATUS[0]}"
      ;;
    mutations) "$here/mutations.sh" chacha-remote aes128-remote chacha-server aes128-server ;;
    gates)
      python3 scripts/check-development-policy
      echo "policy_exit=$?"
      cargo fmt --all --check
      echo "fmt_exit=$?"
      cargo clippy -p shards-remote --all-targets -- -D warnings 2>&1 | quiet
      echo "clippy_remote_x86_64_gnu_exit=${PIPESTATUS[0]}"
      cargo clippy -p shards-server --target x86_64-unknown-linux-gnu --all-targets -- -D warnings 2>&1 | quiet
      echo "clippy_server_x86_64_gnu_exit=${PIPESTATUS[0]}"
      for arch in x86_64 aarch64; do
        env "CC_${arch}_unknown_linux_musl=$PWD/scripts/zig-cc" "AR_${arch}_unknown_linux_musl=$PWD/scripts/zig-ar" \
          "ZIG_TARGET=${arch}-linux-musl" \
          cargo clippy -p shards-server --target "${arch}-unknown-linux-musl" --all-targets -- -D warnings 2>&1 | quiet
        echo "clippy_server_${arch}_musl_exit=${PIPESTATUS[0]}"
      done
      ;;
    *) echo "no stage $1" ;;
  esac
}
for s in "$@"; do
  stage "$s" > "$logs/$s.log" 2>&1
  cat "$logs/$s.log"
  echo "SHA256 $s.log $(sha256sum < "$logs/$s.log" | cut -d' ' -f1)"
done
