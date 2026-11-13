#!/usr/bin/env bash
# Fetch and verify the publishers' own Linux builds of the Codex CLI and Claude Code for the
# harness bring-up (G24/P10, D54). Nothing here is committed: run it with an empty directory,
# and pass the binaries it leaves to the examples and the real-VM test.
#
#   fetch.sh OUTDIR [ARCH]  ->  OUTDIR/codex/{codex,codex-code-mode-host}, OUTDIR/claude/claude,
#                               OUTDIR/fetched.json
#
# ARCH is the guest's, amd64 or arm64 (OCI's names): by default this host's, as a guest's is.
# It runs on Linux (amd64, arm64) and macOS (arm64), with curl, tar, jq and gpg. Each of
# OUTDIR/codex and OUTDIR/claude is a harness's bin directory as its example context takes it.
# Codex: each GitHub release asset is checked against its pinned SHA-256 (the release's own
# digest of it), and each binary against OpenAI's keyless cosign bundle, whose certificate must
# name openai/codex's release workflow at this tag, issued by GitHub Actions' OIDC (the
# workflow .github/workflows/rust-release.yml signs each Linux binary with
# .github/actions/linux-code-sign, cosign v3.1.4, `sign-blob --new-bundle-format=false`;
# cosign v3.1.4 reads that bundle without the flag, which it deprecates for verify-blob).
# cosign is OpenAI's signing version, sigstore's build for this host, checked against its
# pinned digest before it runs. Claude Code: Anthropic's documented check (code.claude.com/
# docs/en/setup, "Binary integrity and code signing"): the release signing key's fingerprint,
# a good signature by it on manifest.json, then the binary's SHA-256 in that manifest. Each
# tool's keys and trust data live under OUTDIR, never in the caller's home.
#
# Exit status: 0, verified; 1, a download, a digest or the key refused; 2, its use; 10 to 15,
# the manifest's signature refused, each for what GnuPG said of it (`verdict`).
set -euo pipefail

CODEX_TAG=rust-v0.167.2
CODEX_BASE=https://github.com/openai/codex/releases/download/$CODEX_TAG
CODEX_IDENTITY=https://github.com/openai/codex/.github/workflows/rust-release.yml@refs/tags/$CODEX_TAG
CODEX_ISSUER=https://token.actions.githubusercontent.com

COSIGN_VERSION=v3.1.4
COSIGN_BASE=https://github.com/sigstore/cosign/releases/download/$COSIGN_VERSION

CLAUDE_VERSION=2.1.322
CLAUDE_BASE=https://downloads.claude.ai/claude-code-releases
CLAUDE_KEY_URL=https://downloads.claude.ai/keys/claude-code.asc
CLAUDE_KEY_FPR=31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE

# The builds of the guest's architecture ARCH: each Codex asset (the CLI, and the code-mode
# host it starts from its own directory, which OpenAI's package archive carries beside it)
# with its archive's and its bundle's SHA-256, GitHub's `digest` of each asset of the release
# (api.github.com repos/openai/codex/releases/tags/rust-v0.167.2: x86_64's read 2026-11-11,
# aarch64's 2026-11-13); and Claude Code's platform, whose SHA-256 is in its signed manifest.
release() {
    case $1 in
    amd64)
        CODEX_ASSET=codex-x86_64-unknown-linux-musl
        CODEX_TGZ_SHA256=8246e846c550ea21197fcc4067bc270dca895e94ea42ed6a282e6f4e0125b15f
        CODEX_BUNDLE_SHA256=eb5bd5e042a0c57b405340720b67a728e5d3ce43b1e5033819facfabd7131c3f
        HOST_ASSET=codex-code-mode-host-x86_64-unknown-linux-musl
        HOST_TGZ_SHA256=189475a654d82ca776e08dfef22734106d3e5b2cf21da6e84ec3d974d02c39a6
        HOST_BUNDLE_SHA256=81100cd9c570b5e35503429a2b33e97c5f5b8658cdae03a8b3f06ee865a7c322
        CLAUDE_PLATFORM=linux-x64-musl
        ;;
    arm64)
        CODEX_ASSET=codex-aarch64-unknown-linux-musl
        CODEX_TGZ_SHA256=ed81fa2c7d209689af061b6f305ed554218de92a1c2c4019142b28f66fa64add
        CODEX_BUNDLE_SHA256=cd67e2bcbbf43b1e08e41bf58a69c747732454f047c452d8826068133a0088b2
        HOST_ASSET=codex-code-mode-host-aarch64-unknown-linux-musl
        HOST_TGZ_SHA256=3c8843f105cb75a267e05057fc841cdf4a00ff4a68510c0f9f7eb8fb129f82ce
        HOST_BUNDLE_SHA256=95bef991dba93be803225a71d9814b2af5c2e95a4152df53d7efdc18112bd344
        CLAUDE_PLATFORM=linux-arm64-musl
        ;;
    *)
        echo "fetch.sh: no build for $1: amd64 or arm64" >&2
        return 2
        ;;
    esac
}

# This host's architecture by OCI's name, from uname's.
arch_of() {
    case $1 in
    x86_64 | amd64) echo amd64 ;;
    aarch64 | arm64) echo arm64 ;;
    *) echo "$1" ;;
    esac
}

# The cosign to run on a host of system $1 (uname -s) and machine $2 (uname -m), with its
# SHA-256: GitHub's `digest` of that asset of sigstore's v3.1.4 release (api.github.com
# repos/sigstore/cosign/releases/tags/v3.1.4, read 2026-11-13; linux-amd64's is v1's recorded
# cosign_checksums.txt line, fetch/fetched.json).
cosign_for() {
    case $1/$(arch_of "$2") in
    Linux/amd64)
        COSIGN_ASSET=cosign-linux-amd64
        COSIGN_SHA256=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa
        ;;
    Linux/arm64)
        COSIGN_ASSET=cosign-linux-arm64
        COSIGN_SHA256=ade83b7aac62e131cacfe96d958948b2150cc9efec46a570a2649b8e27dedad2
        ;;
    Darwin/arm64)
        COSIGN_ASSET=cosign-darwin-arm64
        COSIGN_SHA256=9f9b8cd557cf2e5b3df3373a00baa005a86b02391e3e06dd0ce37b3a1533d3a4
        ;;
    *)
        echo "fetch.sh: no cosign for a $1 $2 host" >&2
        return 2
        ;;
    esac
}

get() { curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"; }
sha() {
    if command -v sha256sum > /dev/null; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi | cut -d' ' -f1
}
size() { wc -c < "$1" | tr -d ' '; }
must() {
    local want=$1 file=$2 got
    got=$(sha "$file")
    if [ "$got" != "$want" ]; then
        echo "fetch.sh: $file: sha256 $got, expected $want" >&2
        exit 1
    fi
}

# What GnuPG's status lines (STATUS, from --status-fd) say of a detached signature, against the
# key FPR (its primary key's fingerprint): 0 only for a good signature by that key. GnuPG writes
# them in g10/mainproc.c's check_sig_and_print (gnupg master ab9c746e9): of a signature that
# verified or did not, BADSIG where it did not (lines 2363-2364), EXPSIG where the signature has
# expired (2365-2366), EXPKEYSIG where its key has (2367-2368), REVKEYSIG where its key is
# revoked (2369-2370), else GOODSIG (2371-2372); VALIDSIG, with the signing key's fingerprint
# and the primary key's last, beside each of those but BADSIG (`!rc`, 2581-2594); and ERRSIG
# where it could not be checked, its key missing (NO_PUBKEY) or its algorithm unknown
# (2712-2723). So VALIDSIG alone says nothing of expiry or revocation: a good signature is a
# GOODSIG whose key ID ends the fingerprint a VALIDSIG of FPR's key gives. Each refusal is its
# own status: 10 BADSIG, 11 ERRSIG, 12 EXPSIG, 13 EXPKEYSIG, 14 REVKEYSIG, 15 no good signature
# by FPR's key ([`refusal`] says each).
verdict() {
    awk -v want="$2" '
        $1 != "[GNUPG:]" { next }
        $2 == "BADSIG" { bad = 1 }
        $2 == "ERRSIG" { err = 1 }
        $2 == "EXPSIG" { expsig = 1 }
        $2 == "EXPKEYSIG" { expkey = 1 }
        $2 == "REVKEYSIG" { revkey = 1 }
        $2 == "GOODSIG" { good[$3] = 1 }
        $2 == "VALIDSIG" && $NF == want { valid[$3] = 1 }
        END {
            if (bad) exit 10
            if (err) exit 11
            if (expsig) exit 12
            if (expkey) exit 13
            if (revkey) exit 14
            for (k in good)
                for (f in valid)
                    if (length(f) >= length(k) && substr(f, length(f) - length(k) + 1) == k) exit 0
            exit 15
        }' "$1"
}

refusal() {
    case $1 in
    10) echo "its signature is bad (BADSIG): it does not match what was signed" ;;
    11) echo "its signature could not be checked (ERRSIG): GnuPG holds no key that made it, or cannot use it" ;;
    12) echo "its signature has expired (EXPSIG)" ;;
    13) echo "it was signed by a key that has expired (EXPKEYSIG)" ;;
    14) echo "it was signed by a key that is revoked (REVKEYSIG)" ;;
    15) echo "it has no good signature by the pinned key (no GOODSIG of a VALIDSIG by it)" ;;
    *) echo "its signature's verdict was $1" ;;
    esac
}

# Codex: each binary's archive and bundle checked against their pins, the binary against its
# bundle, then put in OUTDIR/codex under its own name.
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

main() {
    out=${1:?usage: fetch.sh OUTDIR [amd64|arm64] (OUTDIR an empty directory)}
    local arch=${2:-$(arch_of "$(uname -m)")}
    release "$arch" || exit 2
    cosign_for "$(uname -s)" "$(uname -m)" || exit 2
    mkdir -p "$out"
    out=$(cd "$out" && pwd)
    if [ -n "$(ls -A "$out")" ]; then
        echo "fetch.sh: $out is not empty" >&2
        exit 2
    fi

    # cosign, checked against its pin before it runs.
    mkdir "$out/cosign"
    get "$COSIGN_BASE/$COSIGN_ASSET" "$out/cosign/cosign"
    must "$COSIGN_SHA256" "$out/cosign/cosign"
    chmod 0755 "$out/cosign/cosign"

    mkdir "$out/codex" "$out/codex-dl"
    codex_binary "$CODEX_ASSET" "$CODEX_TGZ_SHA256" "$CODEX_BUNDLE_SHA256" codex
    codex_binary "$HOST_ASSET" "$HOST_TGZ_SHA256" "$HOST_BUNDLE_SHA256" codex-code-mode-host

    # Claude Code.
    mkdir "$out/claude" "$out/claude-dl" "$out/gnupg"
    chmod 0700 "$out/gnupg"
    get "$CLAUDE_KEY_URL" "$out/claude-dl/claude-code.asc"
    get "$CLAUDE_BASE/$CLAUDE_VERSION/manifest.json" "$out/claude-dl/manifest.json"
    get "$CLAUDE_BASE/$CLAUDE_VERSION/manifest.json.sig" "$out/claude-dl/manifest.json.sig"
    gpg --homedir "$out/gnupg" --batch --quiet --import "$out/claude-dl/claude-code.asc" 2> /dev/null
    local fprs status=0
    fprs=$(gpg --homedir "$out/gnupg" --batch --with-colons --fingerprint | awk -F: '$1 == "fpr" {print $10}')
    printf '%s\n' "$fprs" | grep -qx "$CLAUDE_KEY_FPR" || {
        echo "fetch.sh: Claude Code key fingerprints: $fprs" >&2
        exit 1
    }
    # GnuPG's own exit says less than its status lines (0 for a revoked or an expired key's
    # signature, 1 for an expired one): the verdict is theirs.
    gpg --homedir "$out/gnupg" --batch --status-fd 1 --verify \
        "$out/claude-dl/manifest.json.sig" "$out/claude-dl/manifest.json" \
        > "$out/claude-dl/verify.status" 2> "$out/claude-dl/verify.log" || true
    verdict "$out/claude-dl/verify.status" "$CLAUDE_KEY_FPR" || status=$?
    if [ "$status" -ne 0 ]; then
        echo "fetch.sh: Claude Code's manifest.json: $(refusal "$status")" >&2
        cat "$out/claude-dl/verify.status" "$out/claude-dl/verify.log" >&2
        exit "$status"
    fi
    local manifest_version claude_sha claude_size
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
        --arg arch "$arch" \
        --arg codex_tag "$CODEX_TAG" \
        --arg codex_url "$CODEX_BASE/$CODEX_ASSET.tar.gz" \
        --arg codex_tgz "$CODEX_TGZ_SHA256" \
        --arg codex_bundle_url "$CODEX_BASE/$CODEX_ASSET.sigstore" \
        --arg codex_bundle "$CODEX_BUNDLE_SHA256" \
        --arg codex_identity "$CODEX_IDENTITY" \
        --arg codex_issuer "$CODEX_ISSUER" \
        --arg codex_bin "$(sha "$out/codex/codex")" \
        --arg codex_size "$(size "$out/codex/codex")" \
        --arg host_url "$CODEX_BASE/$HOST_ASSET.tar.gz" \
        --arg host_tgz "$HOST_TGZ_SHA256" \
        --arg host_bundle "$HOST_BUNDLE_SHA256" \
        --arg host_bin "$(sha "$out/codex/codex-code-mode-host")" \
        --arg host_size "$(size "$out/codex/codex-code-mode-host")" \
        --arg cosign "$COSIGN_BASE/$COSIGN_ASSET" \
        --arg cosign_sha "$COSIGN_SHA256" \
        --arg claude_version "$CLAUDE_VERSION" \
        --arg claude_platform "$CLAUDE_PLATFORM" \
        --arg claude_url "$CLAUDE_BASE/$CLAUDE_VERSION/$CLAUDE_PLATFORM/claude" \
        --arg claude_manifest "$(sha "$out/claude-dl/manifest.json")" \
        --arg claude_sig "$(sha "$out/claude-dl/manifest.json.sig")" \
        --arg claude_key "$CLAUDE_KEY_FPR" \
        --arg claude_bin "$claude_sha" \
        --arg claude_size "$claude_size" \
        '{arch: $arch,
          codex: {tag: $codex_tag, url: $codex_url, archive_sha256: $codex_tgz,
                  bundle_url: $codex_bundle_url, bundle_sha256: $codex_bundle,
                  signer_identity: $codex_identity, signer_issuer: $codex_issuer,
                  binary_sha256: $codex_bin, binary_size: ($codex_size | tonumber),
                  code_mode_host: {url: $host_url, archive_sha256: $host_tgz,
                                   bundle_sha256: $host_bundle, binary_sha256: $host_bin,
                                   binary_size: ($host_size | tonumber)},
                  verified_with: {cosign: $cosign, sha256: $cosign_sha}},
          claude: {version: $claude_version, platform: $claude_platform, url: $claude_url,
                   manifest_sha256: $claude_manifest, manifest_sig_sha256: $claude_sig,
                   signing_key_fingerprint: $claude_key,
                   binary_sha256: $claude_bin, binary_size: ($claude_size | tonumber)}}' \
        > "$out/fetched.json"
    cat "$out/fetched.json"
}

# Run, unless sourced (fetch-test.sh holds the functions above to its fixtures).
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
