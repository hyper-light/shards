#!/bin/sh
# PM M385 (D202): what a GPU's VM process needs of Windows to reach its GPU's driver. With a
# `shards` built from confinement.patch (and SHARDS_GPU_LIBS), in a home that has IMAGE (an
# image with Mesa's Venus and vulkaninfo, as tests/gpu.rs builds), each variant: a daemon of
# its own started with the variant's switches, one GPU run of IMAGE's vulkaninfo, what that
# found, and what the GPU's VM process found of Windows' graphics as its renderer started
# (the M385 lines of the daemon's log). Then a window station and desktop's making, timed
# 200 times. Git Bash on Windows.
#   SHARDS_HOME=HOME variants.sh SHARDS IMAGE
set -u
shards=$1 image=$2
log=$SHARDS_HOME/daemon.log
variant() {
	name=$1
	shift
	"$shards" stop daemon >/dev/null 2>&1
	: >"$log"
	echo "== $name"
	env "$@" "$shards" run --rm --gpus all "$image" vulkaninfo --summary 2>&1 | grep -E 'deviceName|ended before'
	grep -E 'M385|Found no drivers' "$log" | sort -u
}
variant "as shards makes it: its AppContainer, a window station and desktop of its own, Win32k allowed"
variant "Win32k disabled" SHARDS_M385_WIN32K_DISABLED=1
variant "no window station or desktop of its own" SHARDS_M385_NO_STATION=1
variant "neither" SHARDS_M385_NO_STATION=1 SHARDS_M385_WIN32K_DISABLED=1
variant "no AppContainer, Win32k disabled" SHARDS_M385_NO_APP_CONTAINER=1 SHARDS_M385_WIN32K_DISABLED=1
variant "no AppContainer, Win32k allowed" SHARDS_M385_NO_APP_CONTAINER=1
"$shards" stop daemon >/dev/null 2>&1
: >"$log"
SHARDS_M385_STATIONS=200 "$shards" run --rm --gpus all "$image" true >/dev/null 2>&1
grep 'M385: stations' "$log"
