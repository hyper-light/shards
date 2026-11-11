# Sourced by the runner scripts: the build environment, and a check that this tree is the local
# one. kiln's sync sends what changed locally, not what a killed run left patched here.
. docs/research/measurements/audit-fix-tests-v1/logs/linux-build.env
m=docs/research/measurements/audit-fix-tests-v1/mutations
tree() { git diff | sha256sum | cut -d' ' -f1; }
if [ "$(tree)" != "${EXPECTED_DIFF:?}" ]; then
	for p in "$m"/*.patch; do
		if patch -p1 -R --dry-run -s <"$p" >/dev/null 2>&1; then
			patch -p1 -R -s <"$p" && echo "reverted a mutation left applied: $(basename "$p")"
		fi
	done
fi
if [ "$(tree)" != "$EXPECTED_DIFF" ]; then
	echo "this tree is not the local one: $(tree) != $EXPECTED_DIFF"
	exit 1
fi
echo "tree matches the local diff $EXPECTED_DIFF at $(git rev-parse HEAD)"
# Files a mutation patches, newer than any binary built from them: kiln's sync restores the
# local (older) mtime, and cargo would otherwise keep an earlier run's mutant build.
targets() { sed -n 's|^+++ b/||p' "$@" | sort -u; }
touch $(targets "$m"/*.patch)
