#!/usr/bin/env bash
# Records what GnuPG says of a detached signature (`gpg --status-fd 1 --verify`) in each way one
# checks, for fetch-test.sh to hold fetch.sh's verdict to, offline:
#
#   record.sh DIR    (the fixtures beside this script were recorded with DIR its own directory)
#
# The keys are throwaway ones made here, each in .invalid (RFC 2606), in a GnuPG home of this
# run's own that is removed after it; nothing reaches a network. GnuPG is told the time it is
# (--faked-system-time) to make a key or a signature that has since expired. goodsig.status,
# Anthropic's real manifest and key, is v1's own record (../fetch/verify.status), copied.
set -euo pipefail
out=${1:?usage: record.sh DIR}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
home=$(mktemp -d)
empty=$(mktemp -d)
trap 'gpgconf --homedir "$home" --kill all 2> /dev/null || true; rm -rf "$home" "$empty"' EXIT
chmod 0700 "$home" "$empty"
g() { gpg --homedir "$home" --batch --no-tty --pinentry-mode loopback --passphrase '' "$@"; }
fpr() { g --with-colons --list-keys "$1" | awk -F: '$1 == "fpr" && !f {print $10; f = 1}'; }
key() { g "${@:3}" --quick-gen-key "Shards fetch.sh test <$1@shards.invalid>" ed25519 sign "$2" 2> /dev/null; }
sign() { g "${@:3}" --local-user "$1@shards.invalid" --detach-sign --output "$home/$2.sig" "$home/data"; }
# What GnuPG said of the signature $2 over $3, as $1.status; with the homedir $4.
verify() {
    local status=0
    gpg --homedir "${4:-$home}" --batch --no-tty --status-fd 1 --verify "$home/$2.sig" "$3" \
        > "$out/$1.status" 2> /dev/null || status=$?
    echo "$1: gpg exit $status" >> "$out/recorded.txt"
}
day=86400
then=$(($(date +%s) - 10 * day))
printf 'a manifest\n' > "$home/data"
printf 'another manifest\n' > "$home/altered"
gpg --version | head -1 > "$out/recorded.txt"
cp "$here/../fetch/verify.status" "$out/goodsig.status"
echo "goodsig: v1's record of Anthropic's manifest, ../fetch/verify.status" >> "$out/recorded.txt"

key pinned never
fpr pinned@shards.invalid > "$out/pinned.fpr"
sign pinned good
verify goodsig-throwaway good "$home/data"
# The signature over another manifest than the one signed.
verify badsig good "$home/altered"
# GnuPG with no key that made it.
verify errsig good "$home/data" "$empty"
# A good signature, by a key that is not the pinned one.
key other never
sign other other
verify other-key other "$home/data"
# A signature made ten days ago, to expire a day after it was made.
key older never --faked-system-time "$((then - day))"
sign older expsig --faked-system-time "$then" --default-sig-expire 1d
verify expsig expsig "$home/data"
fpr older@shards.invalid > "$out/older.fpr"
# A key made ten days ago to expire a day after, signing when it was valid.
key expired 1d --faked-system-time "$then"
sign expired expkeysig --faked-system-time "$((then + 3600))"
verify expkeysig expkeysig "$home/data"
fpr expired@shards.invalid > "$out/expired.fpr"
# A key whose revocation, made with it (openpgp-revocs.d, a colon before its armor's first
# five dashes to guard it), is then imported.
key revoked never
sign revoked revkeysig
revoked=$(fpr revoked@shards.invalid)
echo "$revoked" > "$out/revoked.fpr"
sed 's/^:-----BEGIN/-----BEGIN/' "$home/openpgp-revocs.d/$revoked.rev" | g --import 2> /dev/null
verify revkeysig revkeysig "$home/data"
fpr other@shards.invalid > "$out/other.fpr"
cat "$out/recorded.txt"
