# Whether Windows keeps a file's pages cached once the last process that mapped them ends
# (PM M421). A 256 MiB file written past the cache (write.ps1), dense, then sparse with its
# first half a hole (FSCTL_SET_ZERO_DATA); 1,000 random pages of its data, and of the hole,
# touched by one process after another through a copy-on-write view, as a WHP restore maps
# its memory file (vmm platform::windows; touch.ps1), timed: with nothing holding the file,
# with another process holding it open (as snapshot::hold does), holding a view of it, and
# holding a view whose pages it has read (hold.ps1).
#
#   sh run.sh DIR
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
f="$1/memory"
w=$(cygpath -w "$f")
ps() { powershell -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$here/$1")" "${@:2}" < /dev/null | tr -d '\r'; }
t() { ps touch.ps1 "$w" 1000 "$1" "$2"; }
touches() {
  for held in nothing handle view touched; do
    pid=
    case $held in
      handle) ps hold.ps1 "$w" & pid=$! ;;
      view) ps hold.ps1 "$w" -View & pid=$! ;;
      touched) ps hold.ps1 "$w" -View -From "$2" -To "$3" & pid=$! ;;
    esac
    [ -z "$pid" ] || sleep 8
    echo "$1, $held held: $(t "$2" "$3") $(t "$2" "$3") $(t "$2" "$3") ms"
    [ -z "$pid" ] || { taskkill //F //T //PID "$(cat /proc/$pid/winpid)" > /dev/null 2>&1 || true; wait "$pid" 2> /dev/null || true; }
    # A fresh file for each case: what one case left cached is none of the next's.
    rm -f "$f"
    ps write.ps1 "$w" 256
    [ "$4" = dense ] || { fsutil sparse setflag "$w" > /dev/null; fsutil sparse setrange "$w" 0 134217728 > /dev/null; }
  done
}
rm -f "$f"
ps write.ps1 "$w" 256
touches "dense, its data" 134217728 268435456 dense
fsutil sparse setflag "$w" > /dev/null
fsutil sparse setrange "$w" 0 134217728 > /dev/null
touches "sparse, its data" 134217728 268435456 sparse
touches "sparse, its hole" 0 134217728 sparse
rm -f "$f"
