#!/usr/bin/env bash
# Peak memory of `shards build` ADDing an archive of ENTRIES empty files (PM M78).
#   run.sh SHARDS DIR [ENTRIES] [RUNS]
# SHARDS is a release `shards`; DIR is scratch space, which holds the context and a
# SHARDS_HOME of its own. Prints each run's wall time and maximum resident set size.
set -euo pipefail
shards=$1 dir=$2 entries=${3:-1000000} runs=${4:-3}
mkdir -p "$dir/context" "$dir/home"
if [ ! -f "$dir/context/many.tar.gz" ]; then
  python3 - "$dir/context/many.tar.gz" "$entries" <<'PY'
import sys, tarfile
with tarfile.open(sys.argv[1], "w:gz", format=tarfile.USTAR_FORMAT) as t:
    for n in range(int(sys.argv[2])):
        t.addfile(tarfile.TarInfo(f"d{n // 100}/f{n}"))
PY
fi
printf 'FROM alpine:3.22\nADD many.tar.gz /x/\n' > "$dir/context/Dockerfile"
case "$(uname)" in Darwin) time=(/usr/bin/time -l) ;; *) time=(/usr/bin/time -v) ;; esac
for _ in $(seq "$runs"); do
  SHARDS_HOME="$dir/home" "${time[@]}" "$shards" build -q "$dir/context" 2>&1 >/dev/null |
    grep -E 'real|Elapsed|[Mm]aximum resident'
done
