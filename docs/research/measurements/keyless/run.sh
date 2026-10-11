#!/bin/sh
# PM M184: keyless signing (D126) held to cosign v3.1.3 against a local Sigstore, in the
# shards-dind container (docker:29.3.1-dind): setup.sh starts Fulcio v1.9.0, Rekor v1.5.4,
# timestamp-authority v2.1.4, rekor-tiles v2.3.0 and a fake OIDC issuer as Sigstore's own
# CI does, and a registry; cases.py signs with shards and with cosign and verifies each
# other's. The bundles and the trusted root land in crates/sigstore/testdata/keyless,
# which crates/sigstore/tests/keyless.rs verifies offline. Everything it started is
# stopped at the end (KEEP=1 leaves the stack up for another run).
#
#   docs/research/measurements/keyless/run.sh SHARDS_LINUX_BINARY [N]
#
# The binary is shards for the container's architecture, static musl (aarch64 on Apple
# silicon): cargo build --profile test-release --target aarch64-unknown-linux-musl -p
# shards, with CC/AR_aarch64_unknown_linux_musl=scripts/zig-cc|zig-ar,
# ZIG_TARGET=aarch64-linux-musl, CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
# and SHARDS_HELPERS=skip.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
shards=$1
n=${2:-10}
dind=shards-dind
inside=/root/keyless
arch=$(docker exec "$dind" uname -m)
case $arch in
aarch64) cosign_arch=arm64 ;;
x86_64) cosign_arch=amd64 ;;
*) echo "no cosign for $arch" >&2; exit 1 ;;
esac

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# cosign's release, its checksum checked against the release's list.
base=https://github.com/sigstore/cosign/releases/download/v3.1.3
curl -sSL -o "$stage/cosign" "$base/cosign-linux-$cosign_arch"
curl -sSL -o "$stage/checksums" "$base/cosign_checksums.txt"
want=$(grep " cosign-linux-$cosign_arch\$" "$stage/checksums" | cut -d' ' -f1)
got=$(shasum -a 256 "$stage/cosign" | cut -d' ' -f1)
[ -n "$want" ] && [ "$want" = "$got" ] || { echo "cosign checksum $got, want $want" >&2; exit 1; }
chmod +x "$stage/cosign"
cp "$shards" "$stage/shards"
cp "$here/setup.sh" "$here/cases.py" "$here/browser.sh" "$here/device-login.sh" "$stage/"
# /tmp is a tmpfs there, out of docker cp's reach: streamed in by tar.
docker exec "$dind" mkdir -p "$inside"
tar -C "$stage" -cf - shards cosign setup.sh cases.py browser.sh device-login.sh |
	docker exec -i "$dind" tar -xf - -C "$inside"

down() {
	[ "${KEEP:-}" = 1 ] && return
	docker exec "$dind" sh -c "
		docker rm -f shards-keyless-registry >/dev/null 2>&1
		for d in $inside/work/services/*/ $inside/work/scaffolding/actions/setup-sigstore-env/fakeoidc/; do
			[ -f \"\$d/docker-compose.yml\" ] || [ -f \"\$d/compose.yml\" ] || continue
			(cd \"\$d\" && docker compose down --volumes >/dev/null 2>&1)
		done" || true
}
trap 'down; rm -rf "$stage"' EXIT
docker exec "$dind" sh "$inside/setup.sh" "$inside/work"
docker exec "$dind" python3 -I "$inside/cases.py" "$inside/work" "$inside/shards" "$inside/cosign" "$inside/out" "$n" \
	>"$stage/results.json"
out="$root/crates/sigstore/testdata/keyless"
rm -rf "$out"
mkdir -p "$out"
docker exec "$dind" tar -C "$inside/out" -cf - . | tar -xf - -C "$out"
cat "$stage/results.json"
