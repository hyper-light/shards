#!/bin/bash
# Step 33, step 7 (D242): which HTTP/2 and HPACK implementation the injector can stand on.
# For each candidate: its crates.io release record, its RustSec advisories, and the lines of its
# source that say whether a field sent never-indexed (RFC 7541 §6.2.3) is still known as one after
# decoding, and whether its encoder can send one (§7.1.3: an intermediary MUST NOT re-encode such a
# value indexed). Then the crates `h2` would add to shards' Cargo.lock. Each download goes to a new,
# empty directory of its own; nothing downloaded is run.
# Usage: step7-h2-survey.sh WORKTREE SCRATCH
set -eu
w=$1 s=$2
mkdir -p "$s"
ua="generic-research-client"
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(uname -sm); Cargo.lock of $(git -C "$w" rev-parse HEAD)"
echo "## crates.io"
for c in h2 hpack fluke-hpack loona-hpack httlib-hpack; do
  curl -s -A "$ua" "https://crates.io/api/v1/crates/$c" | python3 -I -c '
import json, sys
c = json.load(sys.stdin)["crate"]
print("%s: newest %s, last release %s, repository %s" % (c["name"], c["max_version"], c["updated_at"][:10], c["repository"]))'
done
echo "## RustSec (advisory-db $(gh api repos/rustsec/advisory-db/commits/main --jq .sha))"
for c in h2 hpack fluke-hpack loona-hpack httlib-hpack; do
  if ids=$(gh api "repos/rustsec/advisory-db/contents/crates/$c" --jq '.[].name' 2>/dev/null); then
    echo "$c: $(echo "$ids" | tr '\n' ' ')"
  else
    echo "$c: none (no crates/$c in the database)"
  fi
done
echo "## repositories' last push"
for r in hyperium/h2 bearcove/loona xpepermint/httlib-rs mlalic/hpack-rs; do
  echo "$r: $(gh api "repos/$r" --jq .pushed_at)"
done
echo "## sources: a never-indexed field after decoding, and the encoder's representations"
fetch() { mkdir -p "$s/$1" && curl -sL -A "$ua" "https://static.crates.io/crates/$1/$1-$2.crate" -o "$s/$1/c.crate" \
  && echo "$1 $2 sha256 $(shasum -a 256 "$s/$1/c.crate" | cut -d' ' -f1)" && tar -xzf "$s/$1/c.crate" -C "$s/$1"; }
fetch h2 0.4.20
grep -n -A7 'LiteralNeverIndexed => {' "$s/h2/h2-0.4.20/src/hpack/decoder.rs"
grep -n '^mod hpack;' "$s/h2/h2-0.4.20/src/lib.rs"
echo "h2's dependencies:"; grep -o '^\[dependencies\.[a-z_-]*\]' "$s/h2/h2-0.4.20/Cargo.toml" | tr '\n' ' '; echo
echo "h2's chosen limits:"; grep -rn 'const DEFAULT_[A-Z_]*\|MAX_RECV_EMPTY_DATA_FRAMES\|MAX_HEADER_LIST_ABUSE' \
  "$s/h2/h2-0.4.20/src/proto/mod.rs" "$s/h2/h2-0.4.20/src/frame/headers.rs" | sed "s|$s/h2/||"
fetch loona-hpack 0.4.3
grep -n 'cb: impl FnMut' "$s/loona-hpack/loona-hpack-0.4.3/src/decoder.rs"
grep -n -A5 'FieldRepresentation::LiteralNeverIndexed => {' "$s/loona-hpack/loona-hpack-0.4.3/src/decoder.rs"
grep -n -A12 'pub fn encode_header_into' "$s/loona-hpack/loona-hpack-0.4.3/src/encoder.rs" | grep -n 'find_header\|encode_literal\|add_header'
fetch httlib-hpack 0.1.3
grep -n 'pub const NEVER_INDEXED' "$s/httlib-hpack/httlib-hpack-0.1.3/src/encoder/mod.rs"
grep -n 'buf\[total - 1\]\|&buf\[total\.\.\]' "$s/httlib-hpack/httlib-hpack-0.1.3/src/decoder/"*.rs | sed "s|$s/httlib-hpack/||"
echo "## established implementations that keep it"
g=$(gh api repos/golang/net/commits/master --jq .sha)
gh api "repos/golang/net/contents/http2/hpack/hpack.go?ref=$g" --jq .content | base64 -d > "$s/go-hpack.go"
gh api "repos/golang/net/contents/http2/hpack/encode.go?ref=$g" --jq .content | base64 -d > "$s/go-encode.go"
echo "golang.org/x/net $g:"; grep -n 'Sensitive bool\|hf.Sensitive = it.sensitive()' "$s/go-hpack.go"; grep -n 'return !f.Sensitive' "$s/go-encode.go"
n=$(gh api repos/nghttp2/nghttp2/commits/master --jq .sha)
gh api "repos/nghttp2/nghttp2/contents/lib/includes/nghttp2/nghttp2.h?ref=$n" --jq .content | base64 -d > "$s/nghttp2.h"
echo "nghttp2 $n:"; grep -n -A3 'If :enum:`nghttp2_nv_flag.NGHTTP2_NV_FLAG_NO_INDEX` is set in' "$s/nghttp2.h"
echo "## what h2 =0.4.20 adds to shards' Cargo.lock (resolved, not built)"
mkdir -p "$s/probe/src"
printf '[package]\nname = "probe"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nh2 = "=0.4.20"\n' > "$s/probe/Cargo.toml"
echo 'fn main() {}' > "$s/probe/src/main.rs"
(cd "$s/probe" && cargo tree -e normal --prefix none 2>/dev/null) | sed 's/ (\*)//; s/ v/ /' | awk '$1!="probe"{print $1" "$2}' | sort -u > "$s/tree.txt"
awk '$0=="[[package]]"{getline; n=$3; gsub(/"/,"",n); print n}' "$w/Cargo.lock" | sort -u > "$s/lock.txt"
echo "h2's tree: $(wc -l < "$s/tree.txt" | tr -d ' ') crates; new to the lockfile:"
while read -r n v; do grep -qx "$n" "$s/lock.txt" || echo "  $n $v"; done < "$s/tree.txt"
