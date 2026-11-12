#!/bin/sh
# variant.sh NAME SOURCE REVISION: SOURCE (a .tgz of a tree, or a tree) built in a target
# directory of its own, W/target-NAME, so that no other tree's units are taken for its own;
# says whether the VM process and the init it built carry the host fix (0d4f20288) and the
# guest fix (2944d9ab5); then e2e.sh's 10 rounds, logs in W/logs/NAME.
set -u
name=$1 source=$2 rev=$3
here=$(cd "$(dirname "$0")" && pwd)
W=${W:-$HOME/g121-tmp/exec-e2e}
mkdir -p "$W/logs"
case $source in
*.tgz)
	rm -rf "${W:?}/$name"
	mkdir -p "$W/$name"
	tar xzf "$source" -C "$W/$name" || exit 1
	tree=$W/$name
	;;
*) tree=$source ;;
esac
rm -rf "${W:?}/target-$name"
export CARGO_TARGET_DIR="$W/target-$name"
cd "$tree" || exit 1
if ! cargo test --profile test-release -p shards --test containers --no-run >"$W/logs/$name-build.log" 2>&1; then
	tail -30 "$W/logs/$name-build.log"
	exit 1
fi
vm=$(find "$CARGO_TARGET_DIR/helpers" -type f -name shards-vm -path '*test-release*' | head -1)
init=$(find "$CARGO_TARGET_DIR/test-release/build" -type f -name shards-init -path '*/out/*' | head -1)
echo "$name: its VM process carries the host fix: $(strings "$vm" | grep -c 'the exec port refused a guest') ($vm)"
echo "$name: its init carries the guest fix: $(strings "$init" | grep -c "the command's connection to the host failed") ($init)"
REVISION=$rev LOGS=$W/logs/$name sh "$here/e2e.sh" 10
