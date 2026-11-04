#!/bin/sh
# PM M430's run-start A/B (D214): what a run's microVM's PCI slots, empty, cost its run,
# restored from a template that keeps them, against one with none. One build, two
# homes, SHARDS_PCI_SLOTS=0 (old) and 4 (new, the default), each arm's daemon restoring
# its own template; build-ab/ab.py alternates them and pairs each turn's runs.
# Usage: slots-ab.sh SHARDS IMAGE N [COMMAND...]; on macOS through
# macos-sched/launchd-run.sh, as a user's runs are scheduled.
set -eu
shards=$1 image=$2 n=$3
shift 3
here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
config=$(mktemp -d)
export DOCKER_CONFIG=$config
for arm in old new; do
    mkdir -p "$root/$arm"
    # A copy, renamed into place: a signed binary copied over in place is killed at launch.
    cp "$shards" "$root/$arm/shards.tmp" && mv "$root/$arm/shards.tmp" "$root/$arm/shards"
done
SHARDS_HOME=$root/old/home "$root/old/shards" pull "$image" >/dev/null
SHARDS_HOME=$root/old/home "$root/old/shards" daemon stop || true
cp -R "$root/old/home" "$root/new/home"
echo "host: $(uname -a)"
echo "revision: $(git -C "$here" rev-parse HEAD)"
ENV_OLD="SHARDS_PCI_SLOTS=0" ENV_NEW="SHARDS_PCI_SLOTS=4" AB_FLAGS=--rm \
    python3 "$here/../build-ab/ab.py" "$root/old" "$root/new" "$image" "$n" "$@"
rm -rf "$root" "$config"
