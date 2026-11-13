#!/usr/bin/env bash
# fetch.sh's checks held to recorded data, with no download (the real fetch is ROOT's):
#
#   fetch-test.sh [SCRIPT [CHECK]]   SCRIPT: the fetch.sh to hold, by default this one's;
#                                    CHECK: one check, or one signature case, alone
#
# - signatures: its verdict on what GnuPG said of a signature, one recorded status per way a
#   signature checks (fetch-test/*.status: record.sh made them with throwaway keys, and
#   goodsig.status is v1's real record of Anthropic's manifest). A good signature by the pinned
#   key passes; each other is refused with its own exit, in its own words.
# - pins: the builds it takes for amd64 and arm64, and the cosign it runs on each host, each pin
#   its release's own digest of that asset (fetch-test/*.assets, as GitHub's API gives them).
# - cosign-first: run with a stand-in curl, it refuses a cosign that is not sigstore's before
#   it asks for anything else, though the stand-in's checksums file names that cosign.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
script=${1:-$here/fetch.sh}
only=${2:-}
f=$here/fetch-test
failed=0
fail() {
    echo "FAIL ($1): $2"
    failed=1
}
pass() { echo "PASS ($1): $2"; }
wanted() { [ -z "$only" ] || [ "$only" = "$1" ]; }
digest() { awk -v n="$2" '$1 == n {print $2}' "$f/$1"; }

# The signature and pin checks call the script's functions: a script that fetches when it is
# sourced is not sourced.
if grep -qF 'if [ "${BASH_SOURCE[0]}" = "$0" ]; then' "$script"; then
    # shellcheck source=fetch.sh
    source "$script"
    sourced=1
else
    sourced=
fi

# Each recorded status, the key held to it, the exit fetch.sh must give and a word of its
# refusal.
signatures() {
    local name=$1 key=$2 want=$3 word=$4 status=0 said
    wanted "$name" || wanted signatures || return 0
    if [ -z "$sourced" ]; then
        fail "$name" "the script fetches when sourced: no verdict to hold"
        return 0
    fi
    verdict "$f/$name.status" "$key" || status=$?
    said=$(refusal "$status")
    if [ "$status" -ne "$want" ]; then
        fail "$name" "exit $status ($said), not $want"
    elif [ "$want" -ne 0 ] && [[ $said != *"$word"* ]]; then
        fail "$name" "exit $status, but its words, \"$said\", don't say $word"
    else
        pass "$name" "exit $status${word:+: $said}"
    fi
}
pinned=$(cat "$f/pinned.fpr")
# goodsig.status is Anthropic's real signature, held to the key its docs give
# (code.claude.com/docs/en/setup).
signatures goodsig 31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE 0 ""
signatures goodsig-throwaway "$pinned" 0 ""
signatures badsig "$pinned" 10 BADSIG
signatures errsig "$pinned" 11 ERRSIG
signatures expsig "$(cat "$f/older.fpr")" 12 EXPSIG
signatures expkeysig "$(cat "$f/expired.fpr")" 13 EXPKEYSIG
signatures revkeysig "$(cat "$f/revoked.fpr")" 14 REVKEYSIG
signatures other-key "$pinned" 15 "pinned key"

if wanted pins; then
    if [ -z "$sourced" ]; then
        fail pins "the script fetches when sourced: no pins to hold"
    else
        a=codex-rust-v0.167.2.assets
        before=$failed
        for arch in amd64:x86_64:linux-x64-musl arm64:aarch64:linux-arm64-musl; do
            IFS=: read -r oci triple platform <<< "$arch"
            if ! release "$oci" 2> /dev/null; then
                fail pins "$oci: no build"
                continue
            fi
            for pair in "$CODEX_ASSET.tar.gz:$CODEX_TGZ_SHA256" "$CODEX_ASSET.sigstore:$CODEX_BUNDLE_SHA256" \
                "$HOST_ASSET.tar.gz:$HOST_TGZ_SHA256" "$HOST_ASSET.sigstore:$HOST_BUNDLE_SHA256"; do
                asset=${pair%%:*}
                [[ $asset == *-$triple-unknown-linux-musl.* ]] || fail pins "$oci takes $asset"
                [ "$(digest $a "$asset")" = "${pair#*:}" ] || fail pins "$asset pinned ${pair#*:}, its release's digest $(digest $a "$asset")"
            done
            [ "$CLAUDE_PLATFORM" = "$platform" ] || fail pins "$oci takes Claude Code's $CLAUDE_PLATFORM"
            jq -e --arg p "$platform" '.platforms[$p].checksum' "$here/fetch/manifest.json" > /dev/null ||
                fail pins "$platform is not in Claude Code's signed manifest (fetch/manifest.json)"
        done
        if release riscv64 2> /dev/null; then fail pins "riscv64 taken"; fi
        for host in Linux:x86_64:cosign-linux-amd64 Linux:aarch64:cosign-linux-arm64 Darwin:arm64:cosign-darwin-arm64; do
            IFS=: read -r system machine asset <<< "$host"
            if ! cosign_for "$system" "$machine" 2> /dev/null; then
                fail pins "no cosign for $system $machine"
                continue
            fi
            [ "$COSIGN_ASSET" = "$asset" ] || fail pins "$system $machine runs $COSIGN_ASSET"
            [ "$(digest cosign-v3.1.4.assets "$asset")" = "$COSIGN_SHA256" ] ||
                fail pins "$asset pinned $COSIGN_SHA256, its release's digest $(digest cosign-v3.1.4.assets "$asset")"
        done
        # macOS is arm64 alone (R11).
        if cosign_for Darwin x86_64 2> /dev/null; then fail pins "a cosign for an Intel Mac"; fi
        [ "$failed" -ne "$before" ] || pass pins "amd64's and arm64's builds and each host's cosign, each its release's digest"
    fi
fi

if wanted cosign-first; then
    t=$(mktemp -d)
    mkdir "$t/bin"
    printf '#!/bin/sh\ntouch "%s/ran"\n' "$t" > "$t/fake"
    fake=$(if command -v sha256sum > /dev/null; then sha256sum "$t/fake"; else shasum -a 256 "$t/fake"; fi | cut -d' ' -f1)
    # The stand-in curl: a cosign that says so if it runs, a checksums file that names it,
    # and nothing else; each URL it is asked, kept.
    cat > "$t/bin/curl" <<EOF
#!/usr/bin/env bash
out= url=
while [ \$# -gt 0 ]; do
    case \$1 in -o) out=\$2; shift ;; https://*) url=\$1 ;; esac
    shift
done
echo "\$url" >> "$t/asked"
case \$url in
*/cosign_checksums.txt)
    for n in cosign-linux-amd64 cosign-linux-arm64 cosign-darwin-arm64; do echo "$fake  \$n"; done > "\$out" ;;
*/cosign-*) cp "$t/fake" "\$out" ;;
*) exit 22 ;;
esac
EOF
    chmod +x "$t/bin/curl" "$t/fake"
    status=0
    PATH="$t/bin:$PATH" bash "$script" "$t/out" > "$t/said" 2>&1 || status=$?
    case $(uname -s)/$(uname -m) in
    Linux/x86_64) asset=cosign-linux-amd64 ;;
    Linux/aarch64) asset=cosign-linux-arm64 ;;
    *) asset=cosign-darwin-arm64 ;;
    esac
    pin=$(digest cosign-v3.1.4.assets "$asset")
    asked=0
    [ ! -f "$t/asked" ] || asked=$(grep -c "" "$t/asked")
    if [ -e "$t/ran" ]; then
        fail cosign-first "the stand-in cosign ran"
    elif [ "$status" -ne 1 ] || ! grep -q "expected $pin" "$t/said"; then
        fail cosign-first "exit $status, asked $asked URLs ($(tr '\n' ' ' < "$t/asked" 2> /dev/null)): $(tail -n 1 "$t/said")"
    elif [ "$asked" -ne 1 ]; then
        fail cosign-first "refused, but after asking $asked URLs"
    else
        pass cosign-first "refused at its pin ($asset), nothing else asked, the stand-in never ran"
    fi
    rm -rf "$t"
fi
exit "$failed"
