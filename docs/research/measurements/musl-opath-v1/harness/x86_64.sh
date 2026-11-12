#!/bin/bash
# The lane's x86_64 checks on a Linux host, run by `systemd-run --user` so that they outlive
# the session that started them: the branch's head cloned from GitHub into a directory of
# its own, then checks.sh in CI's pinned musl image and natively (glibc), one after the
# other in the same checkout. Logs in ~/musl-opath-v1-logs/REV/.
set -u
w=~/musl-opath-v1-work
if [ -d "$w/.git" ]; then
  git -C "$w" fetch -q --depth 1 origin p/musl-opath-v1 && git -C "$w" reset -q --hard FETCH_HEAD
else
  git clone -q --depth 1 --branch p/musl-opath-v1 https://github.com/hyper-light/shards "$w"
fi
cd "$w" || exit 1
rev=$(git rev-parse --short HEAD)
out=~/musl-opath-v1-logs/$rev
mkdir -p "$out" ~/.cache/musl-opath-cargo-musl
echo "rev $rev host $(hostname) $(uname -r) load $(cut -d' ' -f1-3 /proc/loadavg) start $(date -u +%FT%TZ)" > "$out/status"
docker run --rm -v "$PWD:/work" -w /work -v "$HOME/.cache/musl-opath-cargo-musl:/home/shards/.cargo" \
  -e TARGET=x86_64-unknown-linux-musl -e REV="$rev" -e CARGO_BUILD_JOBS=12 \
  -e CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc \
  rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f \
  sh docs/research/measurements/musl-opath-v1/harness/container-musl.sh > "$out/x86_64-musl.log" 2>&1
echo "musl exit $? $(date -u +%FT%TZ)" >> "$out/status"
CARGO_BUILD_JOBS=12 TARGET=x86_64-unknown-linux-gnu REV="$rev" sh docs/research/measurements/musl-opath-v1/harness/checks.sh > "$out/x86_64-gnu.log" 2>&1
echo "gnu exit $? $(date -u +%FT%TZ)" >> "$out/status"
echo done >> "$out/status"
