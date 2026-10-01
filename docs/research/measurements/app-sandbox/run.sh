#!/bin/sh
# Builds the App Sandbox probe (PM M67, M70) and runs the grant trials: an unsandboxed
# parent (this script) makes bookmarks with bookmark.c, or opens descriptors, and hands
# them to the sandboxed probe, which uses each to open its path for reading or writing.
# The files tried live under $HOME: App Sandbox lets a tool read the directory its own
# executable is in, so files beside the probe would be readable with no grant at all.
#   docs/research/measurements/app-sandbox/run.sh WORKDIR
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${1:?usage: run.sh WORKDIR}
mkdir -p "$work"
cd "$work"
clang -O2 -framework CoreFoundation -framework Hypervisor \
    -Wl,-sectcreate,__TEXT,__info_plist,"$here/Info.plist" "$here/probe.c" -o probe
codesign -f -s - -o runtime --entitlements "$here/entitlements.plist" probe 2>/dev/null
clang -O2 -framework CoreFoundation "$here/bookmark.c" -o bookmark
files=$HOME/.shards-probe
rm -rf "$files"
mkdir -p "$files"
file=$files/granted
printf 'x' >"$file"
rw=$(./bookmark "$file")
ro=$(./bookmark "$file" ro)
# One probe per trial: a sandbox extension, once resolved, lasts the process's life.
for trial in "rw r" "rw w" "ro r" "ro w"; do
    set -- $trial
    if [ "$1" = rw ]; then mark=$rw; else mark=$ro; fi
    printf '%s bookmark, %s: ' "$1" "$2"
    ./probe bookmark "$mark@$file@$2" | grep '^bookmark' | sed 's/^bookmark [^:]*: //'
done
# Directories: a file in one read, and one made in it, under each kind of bookmark.
dir=$files/granted-dir
mkdir -p "$dir"
printf 'x' >"$dir/inside"
drw=$(./bookmark "$dir")
dro=$(./bookmark "$dir" ro)
for trial in "rw inside r" "rw made w" "ro inside r" "ro made w"; do
    set -- $trial
    if [ "$1" = rw ]; then mark=$drw; else mark=$dro; fi
    rm -f "$dir/made"
    printf '%s directory bookmark, %s %s: ' "$1" "$2" "$3"
    ./probe bookmark "$mark@$dir/$2@$3" | grep '^bookmark' | sed 's/^bookmark [^:]*: //'
done
# Descriptors: one opened read-only, reached by its number and as /dev/fd/N.
printf 'read-only descriptor: '
./probe fdread 5 read /dev/fd/5 write /dev/fd/5 5<"$file" | grep -v '^HOME\|^main_us' | tr '\n' ' '
echo
# With no grant at all: a file beside the probe, and one under $HOME.
printf x >"$work/beside"
printf 'no grant: '
./probe read "$work/beside" read "$file" | grep -v '^HOME\|^main_us' | tr '\n' ' '
echo
rm -rf "$files"
