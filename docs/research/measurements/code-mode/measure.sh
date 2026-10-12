#!/bin/sh
# What code mode costs (D136, PM M196). First its engine: the engines it chose between,
# each a static binary built as shards' guest binaries are (the host architecture's musl,
# zig's C for QuickJS-NG), each measure run in a microVM, ROUNDS times, the engines
# interleaved within each round; sizes are the binaries'. Then code mode in place, as an
# agent in its microVM times it: MCP calls against a program making them
# (`code_mode_costs_in_microvms`). With this host, its OS, its load and the revision.
#
#   SHARDS_HOME=$(mktemp -d) docs/research/measurements/code-mode/measure.sh SHARDS [ROUNDS]
set -eu
shards=$1 rounds=${2:-5}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git -C "$root" describe --always --dirty --abbrev=8)"
echo "load: $(uptime | sed 's/.*load averages*: //')"
arch=$(uname -m)
case $arch in arm64 | aarch64) triple=aarch64-unknown-linux-musl zig=aarch64-linux-musl ;; *) triple=x86_64-unknown-linux-musl zig=x86_64-linux-musl ;; esac
upper=$(printf %s "$triple" | tr 'a-z-' 'A-Z_')
lower=$(printf %s "$triple" | tr '-' '_')
out=$(mktemp -d)
trap 'rm -r "$out"' EXIT
(
  cd "$here/engines"
  env "CC_$lower=$root/scripts/zig-cc" "AR_$lower=$root/scripts/zig-ar" ZIG_TARGET=$zig \
    "CARGO_TARGET_${upper}_LINKER=rust-lld" \
    cargo build --release --target "$triple" --features boa,qjs --target-dir "$out/target" >&2
)
for b in base boa qjs; do
  cp "$out/target/$triple/release/$b" "$out/$b"
  echo "$b size_bytes=$(wc -c <"$out/$b" | tr -d ' ')"
done
printf 'FROM scratch\nCOPY base boa qjs /\n' >"$out/Dockerfile"
"$shards" build -q -t code-mode-engines:m196 "$out" >/dev/null
for round in $(seq 1 "$rounds"); do
  "$shards" run --rm code-mode-engines:m196 /base | sed "s/^/round=$round /"
  for measure in "create 1000" "calls" "program 200"; do
    for engine in boa qjs; do
      # shellcheck disable=SC2086
      "$shards" run --rm code-mode-engines:m196 /$engine $measure | sed "s/^/round=$round /"
    done
  done
done
(cd "$root" && cargo test --release -p shards --test build -- --ignored --nocapture code_mode_costs_in_microvms 2>&1 |
  grep -E '^mcp-calls=')
echo "load after: $(uptime | sed 's/.*load averages*: //')"
