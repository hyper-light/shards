#!/usr/bin/env bash
# Read-only observation of this runner's live cgroup2 hierarchy (no write, no cgroup made):
# (1) io.stat lines that name a device but carry neither rbytes= nor wbytes= (finding 1's
#     input), with the exact bytes of the first such line;
# (2) for every cgroup, whether io.stat exists against whether its cgroup.controllers lists
#     io (finding 2's premise: the kernel creates io.* files only where io is enabled).
set -u
root=/sys/fs/cgroup
echo "== kernel"; uname -r
echo "== cgroup2 mount"; grep -w cgroup2 /proc/self/mounts
echo "== root cgroup.controllers"; cat "$root/cgroup.controllers"
echo "== device lines with neither rbytes= nor wbytes= (first 20)"
n=0; first=
while IFS= read -r -d '' f; do
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      *rbytes=*|*wbytes=*) ;;
      '') ;;
      *) n=$((n+1)); [ -z "$first" ] && first="$f"
         [ "$n" -le 20 ] && printf '%s: [%s]\n' "$f" "$line" ;;
    esac
  done < "$f"
done < <(find "$root" -name io.stat -print0 2>/dev/null)
echo "total: $n"
if [ -n "$first" ]; then
  echo "== exact bytes of $first"; od -c "$first" | head -20
fi
echo "== lines with exactly one of rbytes=/wbytes= (expected 0)"
find "$root" -name io.stat -print0 2>/dev/null | xargs -0 cat 2>/dev/null \
  | awk '{r=($0 ~ /rbytes=/); w=($0 ~ /wbytes=/); if (r != w) c++} END {print c+0}'
echo "== cgroups by (io in cgroup.controllers, io.stat present)"
find "$root" -type d 2>/dev/null | while IFS= read -r d; do
  c=" $(cat "$d/cgroup.controllers" 2>/dev/null) "
  case "$c" in *" io "*) l=io-listed;; *) l=io-unlisted;; esac
  if [ -e "$d/io.stat" ]; then p=io.stat-present; else p=io.stat-absent; fi
  echo "$l $p"
done | sort | uniq -c
