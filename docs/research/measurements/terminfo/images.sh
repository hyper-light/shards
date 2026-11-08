#!/bin/sh
# Which terminal types an image can describe to its programs: for each IMAGE, the TERMs
# below that its terminfo database has an entry for, where ncurses looks by default
# (terminfo(5): /etc/terminfo, /lib/terminfo, /usr/share/terminfo, an entry at
# DIR/C/NAME, C its first character, or C's hex code on a case-insensitive filesystem),
# run in a microVM of shards'. Usage: images.sh SHARDS IMAGE...
set -eu
shards=$1
shift
terms="xterm xterm-256color xterm-ghostty xterm-kitty alacritty wezterm foot tmux-256color screen-256color vt100 linux dumb"
dir=$(mktemp -d /tmp/terminfo.XXXXXX)
mkdir "$dir/config"
export SHARDS_HOME="$dir/home" DOCKER_CONFIG="$dir/config"
probe='for t in '"$terms"'; do
    c=$(printf %s "$t" | cut -c1); x=$(printf %s "$c" | od -An -tx1 | tr -d " \n")
    for d in /etc/terminfo /lib/terminfo /usr/share/terminfo; do
        if [ -e "$d/$c/$t" ] || [ -e "$d/$x/$t" ]; then printf "%s " "$t"; break; fi
    done
done; echo'
for image in "$@"; do
    printf '%s: ' "$image"
    "$shards" run --rm --pull missing --entrypoint sh "$image" -c "$probe" 2>&1 | tail -n 1
done
"$shards" daemon stop > /dev/null 2>&1 || true
case $dir in /tmp/terminfo.*) rm -rf "$dir" ;; esac
