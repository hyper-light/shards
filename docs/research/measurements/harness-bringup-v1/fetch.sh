#!/usr/bin/env bash
# Fetch and verify the publishers' own Linux x86_64 builds of the Codex CLI and Claude
# Code for the harness bring-up (G24/P10, D54). Nothing here is committed: run it with an
# empty directory, and pass the binaries it leaves to the examples and the real-VM test.
#
#   fetch.sh OUTDIR   ->  OUTDIR/codex/{codex,codex-code-mode-host}, OUTDIR/claude/claude,
#                         OUTDIR/fetched.json
#
# Each of OUTDIR/codex and OUTDIR/claude is a harness's bin directory as its example
# context takes it. Codex: each GitHub release asset is checked against the release's
# SHA-256 digest, and each binary against OpenAI's keyless cosign bundle, whose certificate must name
# openai/codex's release workflow at this tag, issued by GitHub Actions' OIDC (the
# workflow .github/workflows/rust-release.yml signs each Linux binary with
# .github/actions/linux-code-sign, cosign v3.1.4, `sign-blob --new-bundle-format=false`;
# cosign v3.1.4 reads that bundle without the flag, which it deprecates for verify-blob).
# cosign is OpenAI's signing version, from sigstore's release, checked against its
# published checksums. Claude Code: Anthropic's documented check (code.claude.com/docs/en/
# setup, "Binary integrity and code signing"): the release signing key's fingerprint,
# the detached signature on manifest.json, then the binary's SHA-256 in that manifest.
# Each tool's keys and trust data live under OUTDIR, never in the caller's home.
set -euo pipefail

out=${1:?usage: fetch.sh OUTDIR (an empty directory)}
mkdir -p "$out"
out=$(cd "$out" && pwd)
if [ -n "$(ls -A "$out")" ]; then
    echo "fetch.sh: $out is not empty" >&2
    exit 2
fi

CODEX_TAG=rust-v0.167.2
CODEX_BASE=https://github.com/openai/codex/releases/download/$CODEX_TAG
# Each binary's release assets and their digests (GitHub's asset `digest`, read
# 2026-11-11): the CLI, and the code-mode host it starts from its own directory, which
# OpenAI's package archive carries beside it.
CODEX_ASSET=codex-x86_64-unknown-linux-musl
CODEX_TGZ_SHA256=8246e846c550ea21197fcc4067bc270dca895e94ea42ed6a282e6f4e0125b15f
CODEX_BUNDLE_SHA256=eb5bd5e042a0c57b405340720b67a728e5d3ce43b1e5033819facfabd7131c3f
HOST_ASSET=codex-code-mode-host-x86_64-unknown-linux-musl
HOST_TGZ_SHA256=189475a654d82ca776e08dfef22734106d3e5b2cf21da6e84ec3d974d02c39a6
HOST_BUNDLE_SHA256=81100cd9c570b5e35503429a2b33e97c5f5b8658cdae03a8b3f06ee865a7c322
CODEX_IDENTITY=https://github.com/openai/codex/.github/workflows/rust-release.yml@refs/tags/$CODEX_TAG
CODEX_ISSUER=https://token.actions.githubusercontent.com

COSIGN_VERSION=v3.1.4
COSIGN_BASE=https://github.com/sigstore/cosign/releases/download/$COSIGN_VERSION

CLAUDE_VERSION=2.1.322
CLAUDE_PLATFORM=linux-x64-musl
CLAUDE_BASE=https://downloads.claude.ai/claude-code-releases
CLAUDE_KEY_URL=https://downloads.claude.ai/keys/claude-code.asc
CLAUDE_KEY_FPR=31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE

get() { curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
must() {
    local want=$1 file=$2 got
    got=$(sha "$file")
    if [ "$got" != "$want" ]; then
        echo "fetch.sh: $file: sha256 $got, expected $want" >&2
        exit 1
    fi
}

# cosign, checked against sigstore's checksum file for the release.
mkdir "$out/cosign"
get "$COSIGN_BASE/cosign-linux-amd64" "$out/cosign/cosign"
get "$COSIGN_BASE/cosign_checksums.txt" "$out/cosign/cosign_checksums.txt"
cosign_sha=$(awk '$2 == "cosign-linux-amd64" {print $1}' "$out/cosign/cosign_checksums.txt")
[ -n "$cosign_sha" ] || { echo "fetch.sh: no cosign-linux-amd64 in its checksums" >&2; exit 1; }
must "$cosign_sha" "$out/cosign/cosign"
chmod 0755 "$out/cosign/cosign"

# Codex: each binary's archive and bundle checked against the release's digests, the
# binary against its bundle, then put in OUTDIR/codex under its own name.
codex_binary() {
    local asset=$1 tgz_sha=$2 bundle_sha=$3 name=$4 listed
    get "$CODEX_BASE/$asset.tar.gz" "$out/codex-dl/$asset.tar.gz"
    get "$CODEX_BASE/$asset.sigstore" "$out/codex-dl/$asset.sigstore"
    must "$tgz_sha" "$out/codex-dl/$asset.tar.gz"
    must "$bundle_sha" "$out/codex-dl/$asset.sigstore"
    listed=$(tar -tzf "$out/codex-dl/$asset.tar.gz")
    [ "$listed" = "$asset" ] || { echo "fetch.sh: $asset archive lists: $listed" >&2; exit 1; }
    tar -xzf "$out/codex-dl/$asset.tar.gz" -C "$out/codex-dl" --no-same-owner "$asset"
    TUF_ROOT="$out/cosign/tuf" "$out/cosign/cosign" verify-blob \
        --bundle "$out/codex-dl/$asset.sigstore" \
        --certificate-identity "$CODEX_IDENTITY" \
        --certificate-oidc-issuer "$CODEX_ISSUER" \
        "$out/codex-dl/$asset" > "$out/codex-dl/$asset.verify.log" 2>&1 || {
        cat "$out/codex-dl/$asset.verify.log" >&2
        exit 1
    }
    mv "$out/codex-dl/$asset" "$out/codex/$name"
    chmod 0755 "$out/codex/$name"
}
mkdir "$out/codex" "$out/codex-dl"
codex_binary "$CODEX_ASSET" "$CODEX_TGZ_SHA256" "$CODEX_BUNDLE_SHA256" codex
codex_binary "$HOST_ASSET" "$HOST_TGZ_SHA256" "$HOST_BUNDLE_SHA256" codex-code-mode-host

# Claude Code.
mkdir "$out/claude" "$out/claude-dl" "$out/gnupg"
chmod 0700 "$out/gnupg"
get "$CLAUDE_KEY_URL" "$out/claude-dl/claude-code.asc"
get "$CLAUDE_BASE/$CLAUDE_VERSION/manifest.json" "$out/claude-dl/manifest.json"
get "$CLAUDE_BASE/$CLAUDE_VERSION/manifest.json.sig" "$out/claude-dl/manifest.json.sig"
gpg --homedir "$out/gnupg" --batch --quiet --import "$out/claude-dl/claude-code.asc" 2>/dev/null
fprs=$(gpg --homedir "$out/gnupg" --batch --with-colons --fingerprint | awk -F: '$1 == "fpr" {print $10}')
printf '%s\n' "$fprs" | grep -qx "$CLAUDE_KEY_FPR" || {
    echo "fetch.sh: Claude Code key fingerprints: $fprs" >&2
    exit 1
}
# Only a good signature by the documented primary key passes: VALIDSIG's last field
# is the primary key's fingerprint (GnuPG doc/DETAILS).
gpg --homedir "$out/gnupg" --batch --status-fd 1 --verify \
    "$out/claude-dl/manifest.json.sig" "$out/claude-dl/manifest.json" \
    > "$out/claude-dl/verify.status" 2> "$out/claude-dl/verify.log"
awk -v want="$CLAUDE_KEY_FPR" '$2 == "VALIDSIG" && $NF == want {ok = 1} END {exit !ok}' \
    "$out/claude-dl/verify.status" || {
    cat "$out/claude-dl/verify.status" "$out/claude-dl/verify.log" >&2
    exit 1
}
manifest_version=$(jq -r '.version' "$out/claude-dl/manifest.json")
[ "$manifest_version" = "$CLAUDE_VERSION" ] || {
    echo "fetch.sh: manifest.json says version $manifest_version" >&2
    exit 1
}
claude_sha=$(jq -r --arg p "$CLAUDE_PLATFORM" '.platforms[$p].checksum // empty' "$out/claude-dl/manifest.json")
claude_size=$(jq -r --arg p "$CLAUDE_PLATFORM" '.platforms[$p].size // empty' "$out/claude-dl/manifest.json")
[[ "$claude_sha" =~ ^[0-9a-f]{64}$ ]] || { echo "fetch.sh: no $CLAUDE_PLATFORM in the manifest" >&2; exit 1; }
get "$CLAUDE_BASE/$CLAUDE_VERSION/$CLAUDE_PLATFORM/claude" "$out/claude-dl/claude"
must "$claude_sha" "$out/claude-dl/claude"
mv "$out/claude-dl/claude" "$out/claude/claude"
chmod 0755 "$out/claude/claude"

jq -n \
    --arg codex_tag "$CODEX_TAG" \
    --arg codex_url "$CODEX_BASE/$CODEX_ASSET.tar.gz" \
    --arg codex_tgz "$CODEX_TGZ_SHA256" \
    --arg codex_bundle_url "$CODEX_BASE/$CODEX_ASSET.sigstore" \
    --arg codex_bundle "$CODEX_BUNDLE_SHA256" \
    --arg codex_identity "$CODEX_IDENTITY" \
    --arg codex_issuer "$CODEX_ISSUER" \
    --arg codex_bin "$(sha "$out/codex/codex")" \
    --arg codex_size "$(stat -c %s "$out/codex/codex")" \
    --arg host_url "$CODEX_BASE/$HOST_ASSET.tar.gz" \
    --arg host_tgz "$HOST_TGZ_SHA256" \
    --arg host_bundle "$HOST_BUNDLE_SHA256" \
    --arg host_bin "$(sha "$out/codex/codex-code-mode-host")" \
    --arg host_size "$(stat -c %s "$out/codex/codex-code-mode-host")" \
    --arg cosign "$COSIGN_BASE/cosign-linux-amd64" \
    --arg cosign_sha "$cosign_sha" \
    --arg claude_version "$CLAUDE_VERSION" \
    --arg claude_url "$CLAUDE_BASE/$CLAUDE_VERSION/$CLAUDE_PLATFORM/claude" \
    --arg claude_manifest "$(sha "$out/claude-dl/manifest.json")" \
    --arg claude_sig "$(sha "$out/claude-dl/manifest.json.sig")" \
    --arg claude_key "$CLAUDE_KEY_FPR" \
    --arg claude_bin "$claude_sha" \
    --arg claude_size "$claude_size" \
    '{codex: {tag: $codex_tag, url: $codex_url, archive_sha256: $codex_tgz,
              bundle_url: $codex_bundle_url, bundle_sha256: $codex_bundle,
              signer_identity: $codex_identity, signer_issuer: $codex_issuer,
              binary_sha256: $codex_bin, binary_size: ($codex_size | tonumber),
              code_mode_host: {url: $host_url, archive_sha256: $host_tgz,
                               bundle_sha256: $host_bundle, binary_sha256: $host_bin,
                               binary_size: ($host_size | tonumber)},
              verified_with: {cosign: $cosign, sha256: $cosign_sha}},
      claude: {version: $claude_version, platform: "linux-x64-musl", url: $claude_url,
               manifest_sha256: $claude_manifest, manifest_sig_sha256: $claude_sig,
               signing_key_fingerprint: $claude_key,
               binary_sha256: $claude_bin, binary_size: ($claude_size | tonumber)}}' \
    > "$out/fetched.json"
cat "$out/fetched.json"
